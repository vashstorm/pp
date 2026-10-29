use super::*;

#[test]
fn context_task_reports_server_usage_and_estimated_context_both_protocols() {
    for protocol in PROTOCOLS {
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
}

#[test]
fn context_missing_usage_shows_unavailable_without_stale_carryover() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = protocol.start(
            &server,
            dir.path(),
            &[("primary", Some(32768))],
            r#"{"default_model":"fixture/primary"}"#,
        );
        cli.input("first task\n");
        let (request, _) = protocol.receive(&server);
        protocol.usage_reply(request, "first done");
        cli.wait_for(
            Pipe::Stderr,
            &format!("input {USAGE_INPUT} / output {USAGE_OUTPUT}"),
        );
        cli.input("second task\n");
        let (request, body) = protocol.receive(&server);
        // The first exchange is intact in the follow-up request.
        match protocol {
            Protocol::Chat => {
                let messages = body["messages"].as_array().unwrap();
                slash_assert_message(&messages[1], "user", "first task");
                slash_assert_message(&messages[2], "assistant", "first done");
                slash_assert_message(&messages[3], "user", "second task");
                assert_eq!(messages.len(), 4);
            }
            Protocol::Responses => {
                let input = body["input"].as_array().unwrap();
                response_user(&input[0], "first task");
                slash_assert_message(&input[1], "assistant", "first done");
                response_user(&input[2], "second task");
                assert_eq!(input.len(), 3);
            }
        }
        protocol.final_reply(request, "second done");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"first done\nsecond done\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let statuses = context_statuses(&stderr);
        assert_eq!(statuses.len(), 2, "{stderr}");
        assert!(
            statuses[0].contains(&format!("input {USAGE_INPUT} / output {USAGE_OUTPUT}")),
            "{}",
            statuses[0]
        );
        // A request without usage reports unavailable; the previous request's
        // usage is not carried over.
        let last = statuses[1];
        assert!(last.contains("Last request: unavailable"), "{last}");
        assert!(!last.contains(&USAGE_INPUT.to_string()), "{last}");
        no_more(&server);
    }
}

#[test]
fn context_unknown_window_hides_percent_and_reports_inactive_auto_compact() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = protocol.start(
            &server,
            dir.path(),
            &[("primary", None)],
            r#"{"default_model":"fixture/primary"}"#,
        );
        cli.input("remember the sentinel constraint\n");
        let (request, _) = protocol.receive(&server);
        protocol.usage_reply(request, "usage noted");
        cli.wait_for(
            Pipe::Stderr,
            &format!("input {USAGE_INPUT} / output {USAGE_OUTPUT} tokens"),
        );
        no_more(&server);
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"usage noted\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let statuses = context_statuses(&stderr);
        assert_eq!(statuses.len(), 1, "{stderr}");
        let status = statuses[0];
        // Unknown capacity shows no percent and never invents 0%.
        assert!(status.contains(" / ? tokens (estimated)"), "{status}");
        assert!(!status.contains('%'), "{status}");
        // Server usage is still shown next to the unknown window.
        assert!(
            status.contains(&format!(
                "Last request: input {USAGE_INPUT} / output {USAGE_OUTPUT} tokens"
            )),
            "{status}"
        );
        assert!(
            status.contains("Auto compact: inactive (context_window not configured)"),
            "{status}"
        );
    }
}

#[test]
fn context_auto_compact_policy_line_reflects_settings() {
    for (settings, policy) in [
        (
            r#"{"default_model":"fixture/primary","auto_compact":false}"#,
            "Auto compact: off",
        ),
        (
            r#"{"default_model":"fixture/primary","auto_compact_threshold_percent":42}"#,
            "Auto compact: on at 42%",
        ),
    ] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli =
            Protocol::Chat.start(&server, dir.path(), &[("primary", Some(32768))], settings);
        cli.input("plain task\n");
        let (request, _) = Protocol::Chat.receive(&server);
        final_reply(request, "plain done");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"plain done\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let statuses = context_statuses(&stderr);
        assert_eq!(statuses.len(), 1, "{stderr}");
        let status = statuses[0];
        assert!(status.contains(policy), "{status}");
        // With a known window the policy never claims inactive.
        assert!(!status.contains("inactive"), "{status}");
        assert!(status.contains(" / 32768 tokens ("), "{status}");
        no_more(&server);
    }
}

#[test]
fn context_new_and_model_switch_reset_usage_and_change_capacity() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let windows = [
        ("primary", Some(32768u64)),
        // 为新增委托工具预留上下文，本例只验证模型切换与用量重置。
        ("small", Some(10000u64)),
        ("bare", None),
    ];
    let mut cli = Protocol::Chat.start(
        &server,
        dir.path(),
        &windows,
        r#"{"default_model":"fixture/primary"}"#,
    );
    cli.input("first task\n");
    let (request, _) = Protocol::Chat.receive(&server);
    chat_usage_reply(request, "first done");

    cli.input("/model fixture/small\nsecond task\n");
    let (request, body) = Protocol::Chat.receive(&server);
    assert_eq!(body["model"], "fixture-small");
    final_reply(request, "second done");

    cli.input("/model fixture/bare\nthird task\n");
    let (request, body) = Protocol::Chat.receive(&server);
    assert_eq!(body["model"], "fixture-bare");
    final_reply(request, "third done");

    cli.input("/model fixture/primary\nfourth task\n");
    let (request, _) = Protocol::Chat.receive(&server);
    chat_usage_reply(request, "fourth done");

    cli.input("/new\nfifth task\n");
    let (request, body) = Protocol::Chat.receive(&server);
    // /new discards history: only the fresh user message remains.
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    slash_assert_message(&messages[1], "user", "fifth task");
    final_reply(request, "fifth done");

    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        b"first done\nsecond done\nthird done\nfourth done\nfifth done\n"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let statuses = context_statuses(&stderr);
    assert_eq!(statuses.len(), 5, "{stderr}");
    // Primary with usage: known window denominator and server usage.
    assert!(statuses[0].contains(" / 32768 tokens ("), "{}", statuses[0]);
    assert!(
        statuses[0].contains(&format!("input {USAGE_INPUT} / output {USAGE_OUTPUT}")),
        "{}",
        statuses[0]
    );
    // Model switch clears usage and swaps the capacity denominator.
    assert!(statuses[1].contains(" / 10000 tokens ("), "{}", statuses[1]);
    assert!(
        statuses[1].contains("Last request: unavailable"),
        "{}",
        statuses[1]
    );
    assert!(
        !statuses[1].contains(&USAGE_INPUT.to_string()),
        "{}",
        statuses[1]
    );
    // Switching to a model without a configured window degrades gracefully
    // instead of inheriting the previous model's capacity.
    assert!(
        statuses[2].contains(" / ? tokens (estimated)"),
        "{}",
        statuses[2]
    );
    assert!(
        statuses[2].contains("inactive (context_window not configured)"),
        "{}",
        statuses[2]
    );
    // Back on the configured model, usage works again.
    assert!(statuses[3].contains(" / 32768 tokens ("), "{}", statuses[3]);
    assert!(
        statuses[3].contains(&format!("input {USAGE_INPUT} / output {USAGE_OUTPUT}")),
        "{}",
        statuses[3]
    );
    // /new resets usage too.
    assert!(statuses[4].contains(" / 32768 tokens ("), "{}", statuses[4]);
    assert!(
        statuses[4].contains("Last request: unavailable"),
        "{}",
        statuses[4]
    );
    assert!(
        !statuses[4].contains(&USAGE_INPUT.to_string()),
        "{}",
        statuses[4]
    );
    no_more(&server);
}
