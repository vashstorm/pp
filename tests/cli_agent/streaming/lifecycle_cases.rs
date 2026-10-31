use super::*;

#[test]
fn streaming_idle_timeout_keeps_active_text_thinking_and_heartbeats_alive() {
    for protocol in PROTOCOLS {
        for mode in ["text", "thinking", "heartbeat"] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let mut cli = protocol.start_with_settings(
                &server,
                dir.path(),
                Some("keep generating"),
                "request_timeout_seconds: 1\nrate_limit_retry_budget_seconds: 0\nthink_level: high\n",
            );
            let (request, _) = protocol.receive(&server);
            let mut stream = SseReply::new(request);
            let mut text = "started".to_owned();
            let mut thinking = String::new();
            protocol.text(&mut stream, &text);
            cli.wait_for(Pipe::Stdout, &text);
            let started = Instant::now();
            for index in 0..6 {
                // 主动控制发送间隔，使累计生成时间超过闲置阈值；输出消费仍用栅栏确认。
                thread::sleep(Duration::from_millis(250));
                let delta = format!(" part-{index}");
                match mode {
                    "text" => {
                        protocol.text(&mut stream, &delta);
                        text.push_str(&delta);
                        cli.wait_for(Pipe::Stdout, &text);
                    }
                    "thinking" => {
                        match protocol {
                            Protocol::Chat => {
                                stream.event(chat_chunk(json!({"reasoning_content":delta}), None))
                            }
                            Protocol::Responses => stream.event(json!({
                                "type":"response.reasoning_summary_text.delta",
                                "sequence_number":index + 2, "item_id":"r",
                                "output_index":1, "summary_index":0, "delta":delta
                            })),
                        }
                        thinking.push_str(&delta);
                        cli.wait_for(Pipe::Stderr, &thinking);
                    }
                    "heartbeat" => {
                        // SSE 注释不会产生模型事件，但收到字节也应重置闲置等待。
                        let heartbeat = ": keepalive\n\n";
                        write!(stream.writer, "{:x}\r\n{}\r\n", heartbeat.len(), heartbeat)
                            .unwrap();
                        stream.writer.flush().unwrap();
                    }
                    _ => unreachable!(),
                }
            }
            assert!(started.elapsed() > Duration::from_secs(1));
            let mut native = text_output(&text);
            if mode == "thinking" {
                native.as_array_mut().unwrap().push(json!({
                    "type":"reasoning", "id":"r", "status":"completed",
                    "summary":[{"type":"summary_text", "text":thinking}]
                }));
            }
            protocol.complete(&mut stream, native, false);
            stream.finish();
            let output = cli.finish();
            assert!(output.status.success(), "{protocol:?} {mode}: {output:?}");
            assert_eq!(output.stdout, format!("{text}\n").as_bytes());
            assert_clean_pipes(&output);
            no_more(&server);
        }
    }
}

#[test]
fn streaming_idle_timeout_stops_silent_stream_before_or_after_output() {
    for protocol in PROTOCOLS {
        for partial in [false, true] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let mut cli = protocol.start_with_settings(
                &server,
                dir.path(),
                Some("stalled response"),
                "request_timeout_seconds: 1\nrate_limit_retry_budget_seconds: 0\n",
            );
            let (request, _) = protocol.receive(&server);
            let mut stream = SseReply::new(request);
            if partial {
                protocol.text(&mut stream, "partial");
                cli.wait_for(Pipe::Stdout, "partial");
            }
            // 保持连接开放，等待客户端因连续收不到数据而失败，不能靠 EOF 结束。
            let output = cli.finish();
            assert!(
                !output.status.success(),
                "{protocol:?} {partial}: {output:?}"
            );
            assert_eq!(
                output.stdout,
                if partial {
                    b"partial\n".as_slice()
                } else {
                    b""
                }
            );
            assert!(String::from_utf8_lossy(&output.stderr).contains("HTTP request failed"));
            assert_clean_pipes(&output);
            drop(stream);
            no_more(&server);
        }
    }
}

#[test]
fn thinking_is_streamed_when_enabled_without_polluting_stdout() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = protocol.start_with_settings(
            &server,
            dir.path(),
            Some("inspect"),
            r#"{"think_level":"high"}"#,
        );
        let (request, body) = protocol.receive(&server);
        let mut stream = SseReply::new(request);
        let reasoning = json!({"type":"reasoning","id":"r","status":"completed","summary":[{"type":"summary_text","text":"inspect 中"}],"encrypted_content":"opaque-hidden"});
        match protocol {
            Protocol::Chat => stream.event(chat_chunk(
                json!({"role":"assistant","reasoning_text":"inspect 中","reasoning_opaque":"opaque-hidden"}),
                None,
            )),
            Protocol::Responses => {
                assert_eq!(body["reasoning"], json!({"effort":"high","summary":"auto"}));
                stream.event(json!({"type":"response.reasoning_summary_text.delta","sequence_number":1,"item_id":"r","output_index":0,"summary_index":0,"delta":"inspect 中"}));
            }
        }
        cli.wait_for(Pipe::Stderr, "inspect 中");
        cli.assert_running();
        assert!(cli.text(Pipe::Stdout).is_empty());
        match protocol {
            Protocol::Chat => {
                protocol.text(&mut stream, "answer");
                protocol.complete(&mut stream, text_output("answer"), false);
            }
            Protocol::Responses => {
                stream.event(json!({"type":"response.reasoning_summary_text.done","sequence_number":2,"item_id":"r","output_index":0,"summary_index":0,"text":"inspect 中"}));
                stream.event(json!({"type":"response.completed","sequence_number":3,"response":response_body(json!([reasoning,response_message("m",json!([response_text("answer")]))]))}));
            }
        }
        stream.finish();
        let output = cli.finish();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"answer\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(stderr.matches("inspect 中").count(), 1);
        assert!(stderr.contains("inspect 中\n"));
        assert!(!stderr.contains("opaque-hidden"));
        assert!(!stderr.contains('\x1b'));
    }
}

#[test]
fn streaming_first_chunk_is_visible_before_finish_and_repl_history_is_complete() {
    for protocol in PROTOCOLS {
        for repl in [false, true] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let mut cli = protocol.start(&server, dir.path(), (!repl).then_some("first task"));
            if repl {
                cli.input("first task\n");
            }
            let (request, _) = protocol.receive(&server);
            let mut stream = SseReply::new(request);
            protocol.text(&mut stream, "first-中");
            cli.wait_for(Pipe::Stdout, "first-中");
            cli.assert_running();
            assert!(!cli.text(Pipe::Stdout).contains("-last"));
            protocol.text(&mut stream, "-last");
            protocol.complete(&mut stream, text_output("first-中-last"), false);
            stream.finish();
            if repl {
                cli.input("second task\n");
                let (request, body) = protocol.receive(&server);
                let history = protocol.history(&body);
                match protocol {
                    Protocol::Chat => {
                        assert_eq!(history.len(), 4);
                        assert_eq!(history[2]["role"], "assistant");
                        assert_eq!(history[2]["content"], "first-中-last");
                        assert_eq!(history[3]["content"], "second task");
                    }
                    Protocol::Responses => {
                        assert_eq!(history.len(), 3);
                        assert_eq!(history[1]["content"][0]["text"], "first-中-last");
                        response_user(&history[2], "second task");
                    }
                }
                protocol.final_reply(request, "second-complete");
                cli.input("/quit\n");
            }
            let output = cli.finish();
            assert!(output.status.success(), "{protocol:?}: {:?}", output);
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                if repl {
                    "first-中-last\nsecond-complete\n"
                } else {
                    "first-中-last\n"
                }
            );
            assert_clean_pipes(&output);
            no_more(&server);
        }
    }
}

#[test]
fn streaming_bash_both_pipes_are_live_and_failed_tool_does_not_stop_batch() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = protocol.start(&server, dir.path(), Some("run tools"));
        let (request, _) = protocol.receive(&server);
        let command = "printf 'out-first'; printf 'err-first' >&2; for ((i=0;i<1000;i++)); do [[ -e release ]] && break; sleep 0.01; done; printf 'out-last'";
        protocol.calls_reply(
            request,
            json!([
                protocol.call("live", "bash", json!({"command":command})),
                protocol.call(
                    "bad",
                    "bash",
                    json!({"command":"printf failed-out; printf failed-err >&2; exit 7"})
                ),
                protocol.call(
                    "after",
                    "write",
                    json!({"path":"after-failure","content":"still executed"})
                )
            ]),
        );
        cli.wait_for(Pipe::Stderr, "out-first");
        cli.wait_for(Pipe::Stderr, "err-first");
        cli.assert_running();
        assert!(cli.text(Pipe::Stderr).contains("[bash stdout]"));
        assert!(cli.text(Pipe::Stderr).contains("[bash stderr]"));
        assert!(cli.text(Pipe::Stderr).contains("tool bash: running"));
        assert!(!cli.text(Pipe::Stderr).contains("out-last"));
        assert!(!dir.path().join("after-failure").exists());
        no_more(&server);
        std::fs::write(dir.path().join("release"), "go").unwrap();
        let (request, body) = protocol.receive(&server);
        assert_eq!(
            protocol.tool_result(&body, "live"),
            json!({
                "ok":true, "stdout":"out-firstout-last", "stderr":"err-first",
                "exit_code":0, "timed_out":false, "cancelled":false, "stdout_truncated":false, "stderr_truncated":false
            })
        );
        let failed = protocol.tool_result(&body, "bad");
        assert_eq!(failed["ok"], false);
        assert_eq!(failed["exit_code"], 7);
        assert_eq!(failed["stdout"], "failed-out");
        assert_eq!(failed["stderr"], "failed-err");
        assert_eq!(protocol.tool_result(&body, "after")["ok"], true);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("after-failure")).unwrap(),
            "still executed"
        );
        protocol.final_reply(request, "tools-complete");
        let output = cli.finish();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"tools-complete\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("tool bash: failed"));
        assert_eq!(stderr.matches("out-first").count(), 1);
        assert_eq!(stderr.matches("err-first").count(), 1);
        assert_eq!(stderr.matches("out-last").count(), 1);
        assert_clean_pipes(&output);
        no_more(&server);
    }
}

#[test]
fn streaming_fragmented_calls_wait_for_complete_batch_before_side_effects() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = protocol.start(&server, dir.path(), Some("fragmented tools"));
        let (request, _) = protocol.receive(&server);
        let mut stream = SseReply::new(request);
        let calls = fragmented_calls(protocol, &mut stream, false);
        calls_fence(protocol, &mut stream);
        cli.wait_for(Pipe::Stdout, "batch-gate");
        assert!(!dir.path().join("first-file").exists());
        assert!(!dir.path().join("second-file").exists());
        assert!(!cli.text(Pipe::Stderr).contains("tool write: running"));
        cli.assert_running();
        let mut output = calls.as_array().unwrap().clone();
        output.push(response_message(
            "msg-stream",
            json!([response_text("batch-gate")]),
        ));
        protocol.complete(&mut stream, json!(output), true);
        stream.finish();
        let (request, body) = protocol.receive(&server);
        for (index, expected) in [(0, "first"), (1, "second")] {
            assert_eq!(
                protocol.tool_result(&body, &format!("fragment-{index}"))["ok"],
                true
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join(if index == 0 {
                    "first-file"
                } else {
                    "second-file"
                }))
                .unwrap(),
                expected
            );
        }
        let history = protocol.history(&body);
        match protocol {
            Protocol::Chat => {
                assert_eq!(history[2]["tool_calls"], calls);
                assert_eq!(history[2]["content"], "batch-gate");
                assert_eq!(history[3]["tool_call_id"], "fragment-0");
                assert_eq!(history[4]["tool_call_id"], "fragment-1");
            }
            Protocol::Responses => {
                assert_eq!(history[1], calls[0]);
                assert_eq!(history[2], calls[1]);
            }
        }
        protocol.final_reply(request, "fragment-complete");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"batch-gate\nfragment-complete\n");
        assert_clean_pipes(&output);
        no_more(&server);
    }
}

#[test]
fn streaming_invalid_later_call_rejects_entire_batch_and_repl_history() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = protocol.start(&server, dir.path(), None);
        cli.input("invalid batch\n");
        let (request, _) = protocol.receive(&server);
        let mut stream = SseReply::new(request);
        let mut calls = fragmented_calls(protocol, &mut stream, true)
            .as_array()
            .unwrap()
            .clone();
        calls_fence(protocol, &mut stream);
        cli.wait_for(Pipe::Stdout, "batch-gate");
        assert!(!dir.path().join("first-file").exists());
        calls.push(response_message(
            "msg-stream",
            json!([response_text("batch-gate")]),
        ));
        protocol.complete(&mut stream, json!(calls), true);
        stream.finish();
        cli.input("recover\n");
        let (request, body) = protocol.receive(&server);
        protocol.assert_only_users(&body, &["invalid batch", "recover"]);
        assert!(!dir.path().join("first-file").exists());
        assert!(!dir.path().join("second-file").exists());
        protocol.final_reply(request, "recovered");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"batch-gate\nrecovered\n");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("tool write: running"));
        assert_clean_pipes(&output);
        no_more(&server);
    }
}

#[test]
fn streaming_failures_keep_partial_and_never_commit_when_retries_disabled() {
    for protocol in PROTOCOLS {
        let failures: &[Broken] = match protocol {
            Protocol::Chat => &[
                Broken::Eof,
                Broken::Malformed,
                Broken::Error("private-provider-code"),
                Broken::Error("rate_limit_exceeded"),
                Broken::Failed("error"),
                Broken::Failed("refusal"),
                Broken::Failed("private-provider-reason"),
                Broken::Failed("content_filter"),
                Broken::Incomplete("length"),
            ],
            Protocol::Responses => &[
                Broken::Eof,
                Broken::Malformed,
                Broken::Error("invalid_request"),
                Broken::Error("invalid_request_error"),
                Broken::Error("server_error"),
                Broken::Error("rate_limit_exceeded"),
                Broken::Error("invalid_prompt"),
                Broken::Error("private-provider-code"),
                Broken::Failed("invalid_request"),
                Broken::Failed("invalid_request_error"),
                Broken::Failed("server_error"),
                Broken::Failed("rate_limit_exceeded"),
                Broken::Failed("invalid_prompt"),
                Broken::Failed("private-provider-code"),
                Broken::Incomplete("max_output_tokens"),
                Broken::Incomplete("content_filter"),
                Broken::Incomplete("private-provider-reason"),
            ],
        };
        for &broken in failures {
            for repl in [false, true] {
                let server = server();
                let dir = tempfile::tempdir().unwrap();
                let mut cli = protocol.start_with_settings(
                    &server,
                    dir.path(),
                    (!repl).then_some("broken turn"),
                    "default_model: fixture/primary\nrate_limit_retry_budget_seconds: 0\n",
                );
                if repl {
                    cli.input("broken turn\n");
                }
                let (request, _) = protocol.receive(&server);
                let mut stream = SseReply::new(request);
                protocol.text(&mut stream, "partial-visible");
                cli.wait_for(Pipe::Stdout, "partial-visible");
                cli.assert_running();
                pending_write(protocol, &mut stream);
                break_stream(protocol, &mut stream, broken);
                stream.finish();
                if repl {
                    cli.input("recover\n");
                    let (request, body) = protocol.receive(&server);
                    protocol.assert_only_users(&body, &["broken turn", "recover"]);
                    protocol.final_reply(request, "recovered");
                    cli.input("/quit\n");
                }
                let output = cli.finish();
                assert_eq!(
                    output.status.success(),
                    repl,
                    "{protocol:?} {broken:?}: {output:?}"
                );
                assert_eq!(
                    output.stdout,
                    if repl {
                        b"partial-visible\nrecovered\n".as_slice()
                    } else {
                        b"partial-visible\n".as_slice()
                    }
                );
                assert!(!output.stderr.is_empty(), "missing failure description");
                if matches!(protocol, Protocol::Chat) {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let expected = match broken {
                        Broken::Malformed => Some("invalid JSON syntax"),
                        Broken::Error("rate_limit_exceeded") => Some("rate_limit_exceeded"),
                        Broken::Error(_) => Some("stream error: unrecognized error code"),
                        Broken::Failed("error") => Some("finish_reason=error"),
                        Broken::Failed("refusal") => Some("refusal cannot authorize tool calls"),
                        Broken::Failed("private-provider-reason") => {
                            Some("unsupported enum value at choices[0].finish_reason")
                        }
                        _ => None,
                    };
                    if let Some(expected) = expected {
                        assert!(stderr.contains(expected), "{broken:?}: {stderr}");
                    }
                    assert!(!stderr.contains("private-provider-reason"));
                    assert!(!stderr.contains("private-provider-code"));
                }
                if matches!(protocol, Protocol::Responses) {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let diagnostic = match broken {
                        Broken::Error(code) => Some(("error", code)),
                        Broken::Failed(code) => Some(("response.failed", code)),
                        Broken::Incomplete(reason) => Some(("response.incomplete", reason)),
                        Broken::Eof | Broken::Malformed => None,
                    };
                    if let Some((event, detail)) = diagnostic {
                        assert!(stderr.contains(event), "{broken:?}: {stderr}");
                        match detail {
                            "private-provider-code" | "private-provider-reason" => {}
                            _ => assert!(stderr.contains(detail), "{broken:?}: {stderr}"),
                        }
                        if matches!(detail, "invalid_request" | "invalid_request_error") {
                            assert!(stderr.contains("/effort low"), "{broken:?}: {stderr}");
                        }
                    }
                }
                assert!(!dir.path().join("must-not-exist").exists());
                assert!(!String::from_utf8_lossy(&output.stderr).contains("tool write: running"));
                assert_clean_pipes(&output);
                no_more(&server);
            }
        }
    }
}

#[test]
fn default_reasoning_rejection_allows_manual_effort_recovery_without_committing_failure() {
    let protocol = Protocol::Responses;
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let settings = r#"{"think_level":"default","rate_limit_retry_budget_seconds":0}"#;
    let mut cli = protocol.start_with_settings(&server, dir.path(), None, settings);
    cli.input("original task\n");
    let (request, body) = protocol.receive(&server);
    super::super::think::assert_level(&body, "response", "default");
    protocol.assert_only_users(&body, &["original task"]);
    let mut stream = SseReply::new(request);
    protocol.text(&mut stream, "failed-visible");
    cli.wait_for(Pipe::Stdout, "failed-visible");
    pending_write(protocol, &mut stream);
    break_stream(protocol, &mut stream, Broken::Failed("invalid_request"));
    stream.finish();

    // The failed turn stays in this REPL, but neither an automatic retry nor
    // the queued write may run while the user decides how to recover.
    cli.wait_for(Pipe::Stderr, "invalid_request");
    cli.wait_for(Pipe::Stderr, "/effort low");
    cli.assert_running();
    assert!(!dir.path().join("must-not-exist").exists());
    assert!(!cli.text(Pipe::Stderr).contains("tool write: running"));
    no_more(&server);

    cli.input("/effort low\nrecover task\n");
    let (request, body) = protocol.receive(&server);
    super::super::think::assert_level(&body, "response", "low");
    protocol.assert_only_users(&body, &["original task", "recover task"]);
    assert!(!dir.path().join("must-not-exist").exists());
    protocol.final_reply(request, "recovered");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"failed-visible\nrecovered\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("response.failed"));
    assert!(stderr.contains("invalid_request"));
    assert!(stderr.contains("reasoning.effort"));
    assert!(stderr.contains("/effort low"));
    assert!(!stderr.contains("tool write: running"));
    assert!(!stderr.contains(&server.server_addr().to_string()));
    assert!(!dir.path().join("must-not-exist").exists());
    assert_clean_pipes(&output);
    no_more(&server);
}

#[test]
fn streaming_responses_annotations_default_only_when_missing() {
    for annotations in [
        None,
        Some(json!([])),
        Some(json!([{
            "type":"url_citation", "start_index":0, "end_index":6,
            "url":"https://example.com/source", "title":"source"
        }])),
    ] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = Protocol::Responses.start(&server, dir.path(), None);
        cli.input("first task\n");
        let (request, _) = Protocol::Responses.receive(&server);
        let mut part = json!({"type":"output_text", "text":"answer"});
        if let Some(annotations) = &annotations {
            part["annotations"] = annotations.clone();
        }
        let item = response_message("msg-stream", json!([part]));
        let mut stream = SseReply::new(request);
        stream.event(json!({
            "type":"response.output_item.added", "sequence_number":0,
            "output_index":0, "item":item
        }));
        stream.event(json!({
            "type":"response.content_part.added", "sequence_number":1,
            "item_id":"msg-stream", "output_index":0, "content_index":0,
            "part":part
        }));
        Protocol::Responses.text(&mut stream, "answer");
        stream.event(json!({
            "type":"response.content_part.done", "sequence_number":2,
            "item_id":"msg-stream", "output_index":0, "content_index":0,
            "part":part
        }));
        stream.event(json!({
            "type":"response.output_item.done", "sequence_number":3,
            "output_index":0, "item":item
        }));
        Protocol::Responses.complete(&mut stream, json!([item]), false);
        cli.wait_for(Pipe::Stdout, "answer\n");
        cli.input("second task\n");
        let (request, body) = Protocol::Responses.receive(&server);
        assert_eq!(
            body["input"][1]["content"][0]["annotations"],
            annotations.unwrap_or_else(|| json!([]))
        );
        stream.finish();
        Protocol::Responses.final_reply(request, "done");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"answer\ndone\n");
        assert_clean_pipes(&output);
        no_more(&server);
    }
}

#[test]
fn streaming_responses_function_arguments_invalid_or_missing_final_still_fail() {
    for stage in ["added", "done", "completed"] {
        for arguments in [None, Some(Value::Null), Some(json!({})), Some(json!(7))] {
            if stage == "added" && arguments.as_ref().is_none_or(Value::is_null) {
                continue;
            }
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let cli = Protocol::Responses.start(&server, dir.path(), Some("task"));
            let (request, _) = Protocol::Responses.receive(&server);
            let mut item = response_call(
                "private-call",
                "write",
                json!({"path":"must-not-exist", "content":"private-content"}),
            );
            match arguments {
                Some(arguments) => item["arguments"] = arguments,
                None => {
                    item.as_object_mut().unwrap().remove("arguments");
                }
            }
            let mut stream = SseReply::new(request);
            if stage == "completed" {
                // 同时触发正文注释兼容路径，仍不得补齐最终工具参数。
                Protocol::Responses.complete(
                    &mut stream,
                    json!([
                        item,
                        response_message(
                            "msg-stream",
                            json!([{"type":"output_text", "text":"private-text"}])
                        )
                    ]),
                    true,
                );
            } else {
                stream.event(json!({
                    "type":format!("response.output_item.{stage}"), "sequence_number":0,
                    "output_index":0, "item":item
                }));
            }
            stream.finish();
            let output = cli.finish();
            assert!(!output.status.success(), "{stage}: {output:?}");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("decoding Responses response: invalid JSON or response structure")
            );
            assert!(!stderr.contains("private-"));
            assert!(!stderr.contains("tool write: running"));
            assert!(output.stdout.is_empty());
            assert!(!dir.path().join("must-not-exist").exists());
            no_more(&server);
        }
    }
}

#[test]
fn streaming_responses_annotations_invalid_values_still_fail() {
    for part in [
        json!({"type":"output_text", "text":"private-text", "annotations":null}),
        json!({"type":"output_text", "text":"private-text", "annotations":{}}),
        json!({"type":"output_text", "text":"private-text", "annotations":"private-value"}),
        json!({"type":"output_text", "text":"private-text", "annotations":[{}]}),
        json!({"type":"output_text"}),
    ] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = Protocol::Responses.start(&server, dir.path(), Some("task"));
        let (request, _) = Protocol::Responses.receive(&server);
        let mut stream = SseReply::new(request);
        Protocol::Responses.complete(
            &mut stream,
            json!([response_message("msg-stream", json!([part]))]),
            false,
        );
        stream.finish();
        let output = cli.finish();
        assert!(!output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("decoding Responses response: invalid JSON or response structure"));
        assert!(!stderr.contains("private-"));
        no_more(&server);
    }
}

#[test]
fn streaming_completed_only_text_and_missing_tail_are_printed_once() {
    for prefix in [None, Some("prefix-中")] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = Protocol::Responses.start(&server, dir.path(), None);
        cli.input("first task\n");
        let (request, _) = Protocol::Responses.receive(&server);
        let mut stream = SseReply::new(request);
        if let Some(prefix) = prefix {
            Protocol::Responses.text(&mut stream, prefix);
            cli.wait_for(Pipe::Stdout, prefix);
        }
        Protocol::Responses.complete(&mut stream, text_output("prefix-中-tail"), false);
        // Intentionally leave this HTTP response open. completed, not EOF or
        // [DONE], must release the turn and allow the next REPL request.
        cli.wait_for(Pipe::Stdout, "prefix-中-tail\n");
        cli.input("second task\n");
        let (request, body) = Protocol::Responses.receive(&server);
        assert_eq!(body["input"][1]["content"][0]["text"], "prefix-中-tail");
        stream.finish();
        Protocol::Responses.final_reply(request, "second-complete");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success());
        assert_eq!(
            output.stdout,
            b"prefix-\xe4\xb8\xad-tail\nsecond-complete\n"
        );
        assert_clean_pipes(&output);
        no_more(&server);
    }
}

#[test]
fn streaming_refusal_is_printed_once_and_content_has_priority() {
    for protocol in PROTOCOLS {
        for content in [None, Some("preferred-content")] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let cli = protocol.start(&server, dir.path(), Some("refusal selection"));
            let (request, _) = protocol.receive(&server);
            let mut stream = SseReply::new(request);
            match protocol {
                Protocol::Chat => {
                    stream.event(chat_chunk(
                        json!({"role":"assistant","refusal":"provisional-refusal"}),
                        None,
                    ));
                    if let Some(content) = content {
                        protocol.text(&mut stream, content);
                    }
                }
                Protocol::Responses => {
                    stream.event(json!({"type":"response.refusal.delta","sequence_number":1,
                        "item_id":"msg-stream","output_index":0,"content_index":0,"delta":"provisional-refusal"}));
                    stream.event(
                        json!({"type":"response.content_part.done","sequence_number":2,
                        "item_id":"msg-stream","output_index":0,"content_index":0,
                        "part":{"type":"refusal","refusal":"provisional-refusal"}}),
                    );
                    if let Some(content) = content {
                        stream.event(json!({"type":"response.output_text.delta","sequence_number":3,
                            "item_id":"msg-stream","output_index":0,"content_index":1,"delta":content}));
                    }
                }
            }
            let mut parts = vec![json!({"type":"refusal","refusal":"provisional-refusal"})];
            if let Some(content) = content {
                parts.push(response_text(content));
            }
            protocol.complete(
                &mut stream,
                json!([response_message("msg-stream", json!(parts))]),
                false,
            );
            stream.finish();
            let output = cli.finish();
            assert!(output.status.success(), "{protocol:?}: {output:?}");
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                format!("{}\n", content.unwrap_or("provisional-refusal"))
            );
            assert_clean_pipes(&output);
            no_more(&server);
        }
    }
}

#[test]
fn streaming_chat_copilot_missing_object_and_role_keeps_tool_rounds_and_followups() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Protocol::Chat.start(&server, dir.path(), None);
    cli.input("write a file\n");
    let (request, _) = Protocol::Chat.receive(&server);
    let mut stream = SseReply::new(request);
    let mut chunk = chat_chunk(
        json!({"tool_calls":[{
            "index":0,"id":"copilot-call","type":"function","function":{
                "name":"write","arguments":json!({"path":"copilot-result","content":"OK"}).to_string()
            }
        }]}),
        Some("tool_calls"),
    );
    chunk.as_object_mut().unwrap().remove("object");
    stream.event(chunk);
    let mut usage = chat_chunk(json!({}), None);
    usage.as_object_mut().unwrap().remove("object");
    usage["choices"] = json!([]);
    usage["usage"] = json!({"prompt_tokens":100,"completion_tokens":10,"total_tokens":110});
    stream.event(usage);
    stream.frame("[DONE]");
    stream.finish();
    let (request, body) = Protocol::Chat.receive(&server);
    let result = Protocol::Chat.tool_result(&body, "copilot-call");
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("copilot-result")).unwrap(),
        "OK"
    );
    let mut stream = SseReply::new(request);
    let mut final_chunk = chat_chunk(json!({"content":"done"}), Some("stop"));
    final_chunk.as_object_mut().unwrap().remove("object");
    stream.event(final_chunk);
    stream.frame("[DONE]");
    stream.finish();
    cli.wait_for(Pipe::Stdout, "done");
    cli.input("continue\n");
    let (request, body) = Protocol::Chat.receive(&server);
    let assistants: Vec<_> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "assistant")
        .collect();
    assert_eq!(assistants.len(), 2);
    assert_eq!(assistants[0]["tool_calls"][0]["id"], "copilot-call");
    assert_eq!(assistants[1]["content"], "done");
    let mut stream = SseReply::new(request);
    let mut followup = chat_chunk(json!({"content":"continued"}), Some("stop"));
    followup.as_object_mut().unwrap().remove("object");
    stream.event(followup);
    stream.frame("[DONE]");
    stream.finish();
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"done\ncontinued\n");
    assert_clean_pipes(&output);
    no_more(&server);
}

#[test]
fn streaming_chat_copilot_refusal_is_printed_once_and_restored_as_text() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Protocol::Chat.start(&server, dir.path(), None);
    cli.input("refuse\n");
    let (request, _) = Protocol::Chat.receive(&server);
    let mut stream = SseReply::new(request);
    Protocol::Chat.text(&mut stream, "Cannot help");
    stream.event(chat_chunk(json!({}), Some("refusal")));
    stream.frame("[DONE]");
    stream.finish();
    cli.wait_for(Pipe::Stdout, "Cannot help");
    cli.input("continue\n");
    let (request, body) = Protocol::Chat.receive(&server);
    let assistant: Vec<_> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "assistant")
        .collect();
    assert_eq!(assistant.len(), 1);
    assert_eq!(assistant[0]["content"], "Cannot help");
    assert!(assistant[0].get("tool_calls").is_none_or(Value::is_null));
    Protocol::Chat.final_reply(request, "done");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"Cannot help\ndone\n");
    assert_clean_pipes(&output);
    no_more(&server);
}

#[test]
fn streaming_completed_only_refusal_is_printed_once() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let cli = Protocol::Responses.start(&server, dir.path(), Some("refuse"));
    let (request, _) = Protocol::Responses.receive(&server);
    let mut stream = SseReply::new(request);
    Protocol::Responses.complete(
        &mut stream,
        json!([response_message(
            "msg-stream",
            json!([
                {"type":"refusal","refusal":"completed-refusal"}
            ])
        )]),
        false,
    );
    stream.finish();
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"completed-refusal\n");
    assert_clean_pipes(&output);
    no_more(&server);
}

#[test]
fn streaming_completed_prefix_conflict_fails_without_tools_or_history() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Protocol::Responses.start(&server, dir.path(), None);
    cli.input("conflicting completion\n");
    let (request, _) = Protocol::Responses.receive(&server);
    let mut stream = SseReply::new(request);
    Protocol::Responses.text(&mut stream, "visible-prefix");
    cli.wait_for(Pipe::Stdout, "visible-prefix");
    Protocol::Responses.complete(
        &mut stream,
        json!([
            response_message(
                "msg-stream",
                json!([response_text("different private-provider-body fixture-key")])
            ),
            response_call(
                "never",
                "write",
                json!({"path":"must-not-exist","content":"wrong"})
            )
        ]),
        true,
    );
    stream.finish();
    cli.input("recover\n");
    let (request, body) = Protocol::Responses.receive(&server);
    Protocol::Responses.assert_only_users(&body, &["conflicting completion", "recover"]);
    assert!(!dir.path().join("must-not-exist").exists());
    Protocol::Responses.final_reply(request, "recovered");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"visible-prefix\nrecovered\n");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("tool write: running"));
    assert_clean_pipes(&output);
    no_more(&server);
}

#[test]
fn streaming_empty_bash_has_no_invented_output_or_assistant_turn() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = protocol.start(&server, dir.path(), Some("empty tool"));
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([protocol.call("empty", "bash", json!({"command":"true"}))]),
        );
        let (request, body) = protocol.receive(&server);
        let tool = protocol.tool_result(&body, "empty");
        assert_eq!(tool["stdout"], "");
        assert_eq!(tool["stderr"], "");
        assert_eq!(tool["exit_code"], 0);
        protocol.final_reply(request, "empty-complete");
        let output = cli.finish();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"empty-complete\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("[bash stdout]"));
        assert!(!stderr.contains("[bash stderr]"));
        assert!(stderr.contains("tool bash: ok"));
        assert_clean_pipes(&output);
        no_more(&server);
    }
}
