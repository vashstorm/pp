use super::*;

#[test]
fn responses_default_and_explicit_protocol_route_and_render_final_content() {
    for (protocol, output, expected) in [
        (
            None,
            json!([
                response_message(
                    "msg-one",
                    json!([response_text("first "), response_text("part")])
                ),
                response_message("msg-two", json!([response_text("; second part")]))
            ]),
            "first part; second part\n",
        ),
        (
            Some("response"),
            json!([response_message(
                "msg-refusal",
                json!([{"type": "refusal", "refusal": "Cannot perform that action."}])
            )]),
            "Cannot perform that action.\n",
        ),
        (
            Some("response"),
            json!([response_message(
                "msg-mixed",
                json!([
                    {"type": "refusal", "refusal": "not the answer"},
                    response_text("available answer")
                ])
            )]),
            "available answer\n",
        ),
    ] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            dir.path(),
            Some("answer"),
            fixture_settings(),
            &response_models(&server, protocol),
        );
        let (request, body) = receive_at(&server, "/v1/responses");
        assert_eq!(authorization(&request), "Bearer fixture-key");
        assert_eq!(body["model"], "fixture-model");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("previous_response_id").is_none());
        assert!(
            body["include"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item == "reasoning.encrypted_content")
        );
        response_user(&body["input"][0], "answer");
        response_reply(request, output);
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, expected.as_bytes());
        no_more(&server);
    }
}

#[test]
fn responses_tool_batches_preserve_output_history_and_reset_instructions() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Cli::start_with(
        dir.path(),
        None,
        fixture_settings(),
        &response_models(&server, None),
    );
    cli.input("create and verify a file\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    let instructions = body["instructions"].clone();
    let initial = body["input"].as_array().unwrap().clone();
    assert_eq!(initial.len(), 1);
    response_user(&initial[0], "create and verify a file");
    let reasoning = json!({
        "type": "reasoning",
        "id": "rs-plan",
        "summary": [{"type": "summary_text", "text": "Check the file after writing."}],
        "content": [{"type": "reasoning_text", "text": "Plan the dependent operations."}],
        "encrypted_content": "opaque-encrypted-reasoning",
        "status": "completed"
    });
    let mut commentary =
        response_message("msg-progress", json!([response_text("Checking the file.")]));
    commentary["phase"] = json!("commentary");
    let mut batch = json!([
        reasoning,
        commentary,
        response_call(
            "w",
            "write",
            json!({"path":"demo.txt","content":"hello from responses\n"})
        ),
        response_call("r", "read", json!({"path":"demo.txt"})),
        response_call("b", "bash", json!({"command":"wc -l < demo.txt"}))
    ]);
    // Function-call status is optional in the SDK schema; explicit unfinished statuses are not.
    batch[3].as_object_mut().unwrap().remove("status");
    response_reply(request, batch.clone());
    let (request, body) = receive_at(&server, "/v1/responses");
    let input = body["input"].as_array().unwrap();
    assert_eq!(body["instructions"], instructions);
    assert_eq!(body["store"], false);
    assert!(body.get("previous_response_id").is_none());
    assert_eq!(&input[..initial.len()], initial.as_slice());
    assert_eq!(&input[1..6], batch.as_array().unwrap().as_slice());
    assert_eq!(response_result(&input[6], "w")["bytes_written"], 21);
    let read = response_result(&input[7], "r");
    assert_eq!(read["ok"], true);
    let bash = response_result(&input[8], "b");
    assert_eq!(bash["ok"], true);
    assert_eq!(bash["exit_code"], 0);
    assert_eq!(bash["stdout"].as_str().unwrap().trim(), "1");
    assert_eq!(
        std::fs::read(dir.path().join("demo.txt")).unwrap(),
        b"hello from responses\n"
    );
    let history = input.clone();
    response_reply(
        request,
        json!([
            response_call("unknown", "missing", json!({})),
            response_call("absent", "read", json!({"path":"repaired.txt"}))
        ]),
    );
    let (request, body) = receive_at(&server, "/v1/responses");
    let input = body["input"].as_array().unwrap();
    assert_eq!(&input[..history.len()], history.as_slice());
    for (index, id) in [(11, "unknown"), (12, "absent")] {
        let error = response_result(&input[index], id);
        assert_eq!(error["ok"], false);
    }
    response_reply(
        request,
        json!([
            response_call(
                "repair",
                "write",
                json!({"path":"repaired.txt","content":"fixed"})
            ),
            response_call("verify", "read", json!({"path":"repaired.txt"}))
        ]),
    );
    let (request, body) = receive_at(&server, "/v1/responses");
    let input = body["input"].as_array().unwrap();
    assert_eq!(response_result(&input[15], "repair")["ok"], true);
    assert_eq!(response_result(&input[16], "verify")["ok"], true);
    let history = input.clone();
    let final_message = response_message(
        "msg-verified",
        json!([response_text("verified: demo.txt; repaired: repaired.txt")]),
    );
    response_reply(request, json!([final_message.clone()]));
    cli.input("which files did you create?\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    let input = body["input"].as_array().unwrap();
    assert_eq!(&input[..history.len()], history.as_slice());
    assert_eq!(input[history.len()], final_message);
    response_user(input.last().unwrap(), "which files did you create?");
    assert_eq!(input.len(), history.len() + 2);
    response_final(request, "demo.txt and repaired.txt");
    cli.input("/new\nstart fresh\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    assert_eq!(body["instructions"], instructions);
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 1);
    response_user(&input[0], "start fresh");
    assert!(body.get("previous_response_id").is_none());
    response_final(request, "fresh answer");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        b"Checking the file.\nverified: demo.txt; repaired: repaired.txt\ndemo.txt and repaired.txt\nfresh answer\n"
    );
    no_more(&server);
}

#[test]
fn responses_invalid_batches_never_write_and_repl_recovers() {
    let valid_write = response_call(
        "write-first",
        "write",
        json!({"path":"must-not-exist","content":"bad"}),
    );
    let valid_read = response_call("read-second", "read", json!({"path":"must-not-exist"}));
    let mut cases = Vec::new();
    for (field, value) in [
        ("call_id", json!("write-first")),
        ("call_id", json!("")),
        ("call_id", json!("  ")),
        ("name", json!("")),
        ("name", json!("  ")),
        ("status", json!("in_progress")),
        ("status", json!("incomplete")),
    ] {
        let mut invalid = valid_read.clone();
        invalid[field] = value;
        cases.push(response_body(json!([valid_write.clone(), invalid])));
    }
    let mut missing = valid_read.clone();
    missing.as_object_mut().unwrap().remove("call_id");
    cases.push(response_body(json!([valid_write.clone(), missing])));
    for status in ["failed", "incomplete", "in_progress", "queued", "cancelled"] {
        let mut invalid = response_body(json!([valid_write.clone()]));
        invalid["status"] = json!(status);
        if status == "failed" {
            invalid["error"] =
                json!({"code":"server_error","message":"provider exposed fixture-key"});
        } else if status == "incomplete" {
            invalid["incomplete_details"] = json!({"reason":"max_output_tokens"});
        }
        cases.push(invalid);
    }
    let mut response_error = response_body(json!([valid_write.clone()]));
    response_error["error"] = json!({"code":"server_error","message":"fixture-key"});
    cases.push(response_error);
    for unsupported in [
        json!({
            "type":"function_call_output",
            "id":"fco-unexpected",
            "call_id":"other",
            "output":"provider must not execute our tools",
            "status":"completed"
        }),
        json!({
            "type":"file_search_call",
            "id":"fs-unsupported",
            "queries":["anything"],
            "status":"completed"
        }),
    ] {
        cases.push(response_body(json!([valid_write.clone(), unsupported])));
    }
    cases.push(response_body(json!([response_message(
        "msg-empty",
        json!([])
    )])));
    let mut incomplete_message = response_message(
        "msg-incomplete",
        json!([response_text("not a completed message")]),
    );
    incomplete_message["status"] = json!("incomplete");
    cases.push(response_body(json!([valid_write, incomplete_message])));
    cases.push(response_body(json!([])));
    cases.push(response_body(json!([{
        "type":"reasoning",
        "id":"rs-only",
        "summary":[],
        "encrypted_content":"no-final-message",
        "status":"completed"
    }])));
    for (index, invalid) in cases.into_iter().enumerate() {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = Cli::start_with(
            dir.path(),
            None,
            fixture_settings(),
            &response_models(&server, Some("response")),
        );
        cli.input("do not execute an invalid batch\n");
        let (request, _) = receive_at(&server, "/v1/responses");
        let mut stream = SseReply::new(request);
        stream.event(json!({"type":"response.completed","sequence_number":0,"response":invalid}));
        stream.finish();
        cli.input("recover\n");
        let (request, body) = receive_at(&server, "/v1/responses");
        assert!(
            !dir.path().join("must-not-exist").exists(),
            "invalid case {index} executed a partial batch"
        );
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2, "invalid case {index} polluted history");
        response_user(&input[0], "do not execute an invalid batch");
        response_user(&input[1], "recover");
        response_final(request, "recovered");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success(), "case {index}");
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("fixture-key"),
            "case {index} leaked credentials"
        );
        assert_eq!(output.stdout, b"recovered\n", "case {index}");
        assert!(!dir.path().join("must-not-exist").exists(), "case {index}");
        no_more(&server);
    }
}
