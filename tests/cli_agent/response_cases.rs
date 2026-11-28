use super::*;

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
    assert_eq!(bash["output"].as_str().unwrap().trim(), "1");
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
