use super::*;

#[test]
fn stats_responses_captures_real_usage_and_keeps_unknown_cache_unknown() {
    for (cached, written) in [
        (Some(40), Some(5)),
        (Some(40), Some(0)),
        (Some(40), None),
        (None, None),
    ] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            dir.path(),
            Some("stats response"),
            fixture_settings(),
            &response_models(&server, Some("response")),
        );
        let (request, first_body) = receive_at(&server, "/v1/responses");
        let output = json!([response_message(
            "msg-stats",
            json!([{"type":"output_text","text":"response done","annotations":[]}])
        )]);
        let mut body = response_body(output);
        if let Some(value) = cached {
            body["usage"] = json!({"input_tokens":50,"output_tokens":10,"total_tokens":60,"input_tokens_details":{"cached_tokens":value},"output_tokens_details":{"reasoning_tokens":0}});
            if let Some(written) = written {
                body["usage"]["input_tokens_details"]["cache_write_tokens"] = json!(written);
            }
        }
        let mut stream = request.start_sse().unwrap();
        stream.event(&json!({"type":"response.output_text.delta","sequence_number":0,"item_id":"msg-stats","output_index":0,"content_index":0,"delta":"response done","logprobs":[]})).unwrap();
        stream
            .event(&json!({"type":"response.completed","sequence_number":1,"response":body}))
            .unwrap();
        stream.finish();
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let records = rows(dir.path());
        assert_eq!(records.len(), 1);
        assert_eq!(contexts(dir.path())[0]["request"], first_body);
        assert_eq!(records[0]["cache"], json!(cached));
        assert_eq!(records[0]["cache_write"], json!(written));
        assert_eq!(
            records[0]["input"],
            if cached.is_some() {
                json!(50)
            } else {
                Value::Null
            }
        );
        assert_eq!(
            records[0]["output"],
            if cached.is_some() {
                json!(10)
            } else {
                Value::Null
            }
        );
        assert!(records[0]["ttft"].is_number());
    }
}

#[test]
fn stats_http_errors_are_counted_once_without_response_body_or_credentials() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            dir.path(),
            Some("stats failure"),
            // A zero rate-limit budget keeps this 429 from retrying, so the
            // single stats row stays one failed request.
            "{\n\"default_model\": \"fixture/primary\",\n\"rate_limit_retry_budget_seconds\": 0,\n}\n",
            &response_models(&server, Some(protocol)),
        );
        let (request, first_body) = receive_at(
            &server,
            if protocol == "completion" {
                "/v1/chat/completions"
            } else {
                "/v1/responses"
            },
        );
        request
            .respond(
                Response::builder()
                    .status(429)
                    .body(Body::from("private-error-body fixture-key"))
                    .unwrap(),
            )
            .unwrap();
        let output = cli.finish();
        assert!(!output.status.success());
        let records = rows(dir.path());
        assert_eq!(records.len(), 1);
        assert_eq!(contexts(dir.path())[0]["request"], first_body);
        assert_eq!(records[0]["status"], "failed");
        let encoded = records[0].to_string();
        assert!(!encoded.contains("private-error-body"));
        assert!(!encoded.contains("fixture-key"));
        assert!(encoded.contains("429"));
        no_more(&server);
    }
}

#[test]
fn stats_unavailable_database_does_not_fail_model_task() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".pp")).unwrap();
    std::fs::write(dir.path().join(".pp/stats"), "blocked directory").unwrap();
    let cli = Cli::start(&server, dir.path(), Some("stats unavailable"));
    let (request, _) = receive(&server);
    final_reply(request, "task still completed");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"task still completed\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("statistics unavailable"));
}

#[test]
fn stats_interrupted_streams_keep_metrics_without_storing_partial_output() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            dir.path(),
            Some("partial statistics"),
            fixture_settings(),
            &response_models(&server, Some(protocol)),
        );
        let (request, first_body) = receive_at(
            &server,
            if protocol == "completion" {
                "/v1/chat/completions"
            } else {
                "/v1/responses"
            },
        );
        let mut stream = request.start_sse().unwrap();
        if protocol == "completion" {
            stream
                .event(&chat_chunk(
                    json!({"role":"assistant","content":"partial model output"}),
                    None,
                ))
                .unwrap();
        } else {
            stream.event(&json!({"type":"response.output_text.delta","sequence_number":0,"item_id":"partial","output_index":0,"content_index":0,"delta":"partial model output","logprobs":[]})).unwrap();
        }
        stream.finish();
        let output = cli.finish();
        assert!(!output.status.success());
        let records = rows(dir.path());
        assert_eq!(records.len(), 1);
        assert_eq!(contexts(dir.path())[0]["request"], first_body);
        assert_eq!(records[0]["status"], "failed");
        // 未提交的流只留下指标：正文、思考与会话条目都不入库。
        assert!(tools(dir.path()).is_empty());
        assert!(records[0]["ttft"].is_number());
        no_more(&server);
    }
}

#[test]
fn stats_truncated_chat_preserves_reported_usage() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let cli = Cli::start(&server, dir.path(), Some("truncated response"));
    let (request, _) = receive(&server);
    let mut stream = request.start_sse().unwrap();
    stream
        .event(&chat_chunk(
            json!({"role":"assistant","content":"truncated text"}),
            None,
        ))
        .unwrap();
    stream
        .event(&chat_chunk(json!({}), Some("length")))
        .unwrap();
    stream.event(&json!({"id":"usage","object":"chat.completion.chunk","created":1,"model":"fixture-model","choices":[],"usage":{"prompt_tokens":120,"completion_tokens":15,"total_tokens":135,"prompt_tokens_details":{"cached_tokens":100,"cache_write_tokens":10}}})).unwrap();
    stream.frame("[DONE]").unwrap();
    stream.finish();
    assert!(!cli.finish().status.success());
    let records = rows(dir.path());
    assert_eq!(records[0]["status"], "failed");
    assert_eq!(records[0]["input"], 120);
    assert_eq!(records[0]["output"], 15);
    assert_eq!(records[0]["cache"], 100);
    assert_eq!(records[0]["cache_write"], 10);
}

#[test]
fn stats_cache_write_extensions_preserve_unknown_zero_and_failed_usage() {
    for protocol in ["completion", "response"] {
        for (extension, expected) in [
            (json!({"cache_write_tokens":0}), Some(0)),
            (json!({"cache_write_tokens":5}), Some(5)),
            (json!({"cache_creation_input_tokens":5}), Some(5)),
            (json!({}), None),
            (json!({"cache_write_tokens":-1}), None),
            (json!({"cache_write_tokens":"5"}), None),
        ] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let cli = Cli::start_with(
                dir.path(),
                Some("cache write failure"),
                fixture_settings(),
                &response_models(&server, Some(protocol)),
            );
            let (request, _) = receive_at(
                &server,
                if protocol == "completion" {
                    "/v1/chat/completions"
                } else {
                    "/v1/responses"
                },
            );
            let mut stream = request.start_sse().unwrap();
            if protocol == "completion" {
                stream
                    .event(&chat_chunk(
                        json!({"role":"assistant","content":"truncated"}),
                        None,
                    ))
                    .unwrap();
                stream
                    .event(&chat_chunk(json!({}), Some("length")))
                    .unwrap();
                let mut usage = json!({"prompt_tokens":50,"completion_tokens":10,"total_tokens":60,"prompt_tokens_details":{"cached_tokens":40}});
                usage
                    .as_object_mut()
                    .unwrap()
                    .extend(extension.as_object().unwrap().clone());
                stream.event(&json!({"id":"usage","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[],"usage":usage})).unwrap();
                stream.frame("[DONE]").unwrap();
            } else {
                let mut body = response_body(json!([]));
                body["status"] = json!("incomplete");
                body["incomplete_details"] = json!({"reason":"max_output_tokens"});
                let mut usage = json!({"input_tokens":50,"output_tokens":10,"total_tokens":60,"input_tokens_details":{"cached_tokens":40},"output_tokens_details":{"reasoning_tokens":0}});
                usage
                    .as_object_mut()
                    .unwrap()
                    .extend(extension.as_object().unwrap().clone());
                body["usage"] = usage;
                stream
                    .event(
                        &json!({"type":"response.incomplete","sequence_number":0,"response":body}),
                    )
                    .unwrap();
            }
            stream.finish();
            assert!(!cli.finish().status.success());
            let records = rows(dir.path());
            assert_eq!(records.len(), 1);
            assert_eq!(records[0]["status"], "failed");
            assert_eq!(records[0]["input"], 50);
            assert_eq!(records[0]["cache_write"], json!(expected));
        }
    }
}
