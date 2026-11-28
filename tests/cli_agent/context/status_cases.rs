use super::*;

#[test]
fn context_task_reports_server_usage_and_estimated_context_both_protocols() {
    let protocol = Protocol::Responses;
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = protocol.start(
        &server,
        dir.path(),
        &[("primary", Some(32768))],
        r#"{"default_model":"fixture/primary"}"#,
    );
    cli.input("remember the sentinel constraint\n");
    let (request, body) = protocol.receive(&server);
    if let Protocol::Chat = protocol {
        // The ordinary request itself asks for usage; there is no separate
        // usage request later.
        assert_eq!(body["stream_options"]["include_usage"], true);
    }
    protocol.usage_reply(request, "usage noted");
    cli.wait_for(
        Pipe::Stderr,
        &format!("input {USAGE_INPUT} / output {USAGE_OUTPUT} tokens"),
    );
    // Displaying usage never produces an extra model request.
    no_more(&server);
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"usage noted\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let statuses = context_statuses(&stderr);
    assert_eq!(statuses.len(), 1, "{stderr}");
    let status = statuses[0];
    assert!(status.contains("Model: fixture/primary"), "{status}");
    assert!(status.contains("Context: ~"), "{status}");
    assert!(status.contains(" / 32768 tokens ("), "{status}");
    assert!(status.contains("%, estimated)"), "{status}");
    assert!(
        status.contains(&format!(
            "Last request: input {USAGE_INPUT} / output {USAGE_OUTPUT} tokens"
        )),
        "{status}"
    );
    assert!(status.contains("Auto compact: on at 80%"), "{status}");
    // The context estimate is computed from the full serialized request
    // (system, tool schemas, history); it is neither the server usage
    // total nor the server input alone, and it is substantial.
    let tokens = context_estimated_tokens(status);
    assert_ne!(tokens, USAGE_INPUT + USAGE_OUTPUT, "{status}");
    assert_ne!(tokens, USAGE_INPUT, "{status}");
    assert!(tokens > USAGE_OUTPUT, "{status}");
}
