use super::*;

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
            let mut stream = request.start_sse().unwrap();
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
fn streaming_invalid_later_call_rejects_entire_batch_and_repl_history() {
    let protocol = Protocol::Responses;
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = protocol.start(&server, dir.path(), None);
    cli.input("invalid batch\n");
    let (request, _) = protocol.receive(&server);
    let mut stream = request.start_sse().unwrap();
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

#[test]
fn streaming_failures_keep_partial_and_never_commit_when_retries_disabled() {
    let protocol = Protocol::Responses;
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
            let mut stream = request.start_sse().unwrap();
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
