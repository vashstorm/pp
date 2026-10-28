use super::*;

#[test]
fn plan_line_commands_never_request_model() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Cli::start(&server, dir.path(), None);
    cli.input("/plan\n/plan view\n/plan new\n/plan off\n/plan invalid\n/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        error
            .matches("/plan requires the TUI; run pp in a terminal.")
            .count(),
        3
    );
    // 旧子命令在解析层拒绝：只报本地错误，不发送模型请求、不改模式。
    assert_eq!(
        error
            .matches("Use /plan to toggle; manage plans with /plan-list.")
            .count(),
        2
    );
    assert!(server.try_recv().unwrap().is_none());
    assert!(!dir.path().join(".pp/plans").exists());
    assert!(!dir.path().join(".pp/drafts").exists());
}

#[test]
fn slash_tips_stays_local_without_creating_a_session() {
    let server = server();
    for available in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let models = if available {
            fixture_models(&server)
        } else {
            "{}".into()
        };
        let mut cli = Cli::start_with(dir.path(), None, fixture_settings(), &models);
        cli.input("/tips\n/tips extra\n/quit\n");
        let output = cli.finish();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("Tips:\n"), "{stderr}");
        assert!(stderr.contains("usage: /tips"), "{stderr}");
        assert!(!dir.path().join(".pp/agent/sessions").exists());
    }
    no_more(&server);
}

#[test]
fn slash_local_commands_and_unavailable_help_never_request_models() {
    let a = server();
    let b = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers": {
        "a": slash_provider(&a, "a", "completion", json!({
            "primary": { "model": "vendor-a"}})),
        "gateway": slash_provider(&b, "gateway", "response", json!({
            "team/secondary": { "model": "vendor-b"}}))}})
    .to_string();
    let mut cli = Cli::start_with(
        dir.path(),
        None,
        r#"{"default_model":"a/primary"}"#,
        &models,
    );
    cli.input("/help\n/model\n/bogus\n/model missing/nope\n/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    for command in ["/help", "/model", "/new", "/quit"] {
        assert!(stderr.contains(command), "{stderr}");
    }
    for value in [
        "a/primary",
        "vendor-a",
        "completion",
        "gateway/team/secondary",
        "vendor-b",
        "response",
        "unknown command; use /help",
        "model not found; use /model to list configured models",
    ] {
        assert!(stderr.contains(value), "{stderr}");
    }
    let listing = stderr.find("a/primary").unwrap();
    assert!(listing < stderr.find("gateway/team/secondary").unwrap());
    assert!(
        stderr
            .lines()
            .any(|line| line.contains('*') && line.contains("a/primary"))
    );
    assert!(!stderr.contains("a-key"));
    assert!(!stderr.contains("gateway-key"));
    no_more(&a);
    no_more(&b);

    let unavailable = tempfile::tempdir().unwrap();
    let mut cli = Cli::start_with(unavailable.path(), None, "{}", "{}");
    cli.input("/help\n/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    for command in ["/help", "/model", "/new", "/quit"] {
        assert!(stderr.contains(command), "{stderr}");
    }
    no_more(&a);
    no_more(&b);
}

#[test]
fn slash_single_prompt_is_an_ordinary_model_task() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let cli = Cli::start(&server, dir.path(), Some("/model"));
    let (request, body) = receive(&server);
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    slash_assert_message(&body["messages"][1], "user", "/model");
    final_reply(request, "ordinary slash prompt answer");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"ordinary slash prompt answer\n");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Current model:"));
    no_more(&server);
}

#[test]
fn slash_file_command_expands_prompt() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    install_command(
        dir.path(),
        "review.md",
        "---\ndescription: Review source\n---\nReview $1. Focus: $@[2:].\n",
    );
    let mut cli = Cli::start(&server, dir.path(), None);
    cli.input("/review auth \"error handling\" tests\n");
    let (request, body) = receive(&server);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    // The model receives the expanded template, exactly, including the
    // trailing newline: not the slash text and not the frontmatter.
    slash_assert_message(
        &messages[1],
        "user",
        "Review auth. Focus: error handling tests.\n",
    );
    assert_ne!(messages[1]["content"], "/review auth");
    final_reply(request, "review noted");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"review noted\n");
    no_more(&server);
}

#[test]
fn slash_file_command_help_and_errors_stay_local() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    install_command(
        dir.path(),
        "review.md",
        "---\ndescription: Review source\n---\nReview $1. Focus: $@[2:].\n",
    );
    // A blank body and a built-in name conflict are excluded from the menu
    // and dispatch, but only produce local diagnostics.
    install_command(
        dir.path(),
        "blank.md",
        "---\ndescription: Nothing here\n---\n\n",
    );
    install_command(
        dir.path(),
        "model.md",
        "---\ndescription: Shadows a built-in\n---\nNever loaded.\n",
    );
    let mut cli = Cli::start(&server, dir.path(), None);
    cli.input("/help\n/bogus\n/help extra\n/blank\n");
    // Help and local errors never reach the model.
    no_more(&server);
    cli.input("/review auth\n");
    let (request, body) = receive(&server);
    let messages = body["messages"].as_array().unwrap();
    // The earlier errors leave no residue in the request history.
    assert_eq!(messages.len(), 2);
    slash_assert_message(&messages[1], "user", "Review auth. Focus: .\n");
    final_reply(request, "review ok");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"review ok\n");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("/review [arguments] — Review source"),
        "{stderr}"
    );
    assert!(!stderr.contains("/blank [arguments]"), "{stderr}");
    assert!(stderr.contains("unknown command; use /help"), "{stderr}");
    assert!(stderr.contains("usage: /help"), "{stderr}");
    assert!(
        stderr.contains("command body must not be blank"),
        "{stderr}"
    );
    assert!(
        stderr.contains("command name conflicts with a built-in: /model"),
        "{stderr}"
    );
    assert!(stderr.contains("command: "), "{stderr}");
    no_more(&server);
}

#[test]
fn slash_single_prompt_with_file_commands_stays_literal() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    install_command(
        dir.path(),
        "review.md",
        "---\ndescription: Review source\n---\nReview $1. Focus: $@[2:].\n",
    );
    // Even a broken template stays silent: -p never scans the directory.
    install_command(
        dir.path(),
        "blank.md",
        "---\ndescription: Nothing here\n---\n\n",
    );
    let cli = Cli::start(&server, dir.path(), Some("/review auth"));
    let (request, body) = receive(&server);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    slash_assert_message(&messages[1], "user", "/review auth");
    final_reply(request, "literal prompt answer");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"literal prompt answer\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("command:"), "{stderr}");
    assert!(!stderr.contains("Review source"), "{stderr}");
    no_more(&server);
}
