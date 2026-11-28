#[test]
fn mcp_tui_runs_tools_in_worker_and_keeps_input_draft_responsive() {
    let provider = server();
    let dir = tempfile::tempdir().unwrap();
    crate::write_settings(dir.path(), super::mcp::settings("call_timeout").to_string()).unwrap();
    std::fs::write(config_path(dir.path(), "models.yml"), fixture_models(&provider)).unwrap();
    let mut pty = Pty::start_with_args(dir.path(), dir.path(), &["--yolo"]);
    pty.ready();
    pty.input("MCP task\r");
    let (request, _) = pty.request(&provider);
    calls_reply(request, json!([call("slow", "mcp__local__echo", json!({"text":"hello"}))]));
    pty.text("MCP local Echo");
    pty.input("keep this draft");
    pty.draft("keep this draft");
    let (request, body) = pty.request(&provider);
    let output = result(body["messages"].as_array().unwrap().last().unwrap(), "slow");
    assert_eq!(output["ok"], false);
    assert!(output["error"].as_str().unwrap().contains("timed out"));
    final_reply(request, "MCP recovered");
    pty.text("MCP recovered");
    pty.ready();
    pty.draft("keep this draft");
    pty.input("\u{3}");
    pty.finish();
    super::mcp::assert_stopped(dir.path());
}

