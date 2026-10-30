#[test]
fn mcp_tui_runs_tools_in_worker_and_keeps_input_draft_responsive() {
    let provider = server();
    let dir = tempfile::tempdir().unwrap();
    crate::write_settings(dir.path(), super::mcp::settings("call_timeout").to_string()).unwrap();
    std::fs::write(config_path(dir.path(), "models.yml"), fixture_models(&provider)).unwrap();
    let mut pty = Pty::start_with_existing_files(dir.path());
    pty.ready();
    pty.input("MCP task\r");
    let (request, _) = pty.request(&provider);
    calls_reply(request, json!([call("slow", "mcp__local__echo", json!({"text":"hello"}))]));
    pty.text("mcp__local__echo");
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

#[test]
fn mcp_tui_exit_reaps_servers_while_idle_waiting_for_model_or_running_tool() {
    for phase in ["idle", "model", "tool"] {
        let provider = server();
        let dir = tempfile::tempdir().unwrap();
        let mut config = super::mcp::settings("call_timeout");
        config["mcpServers"]["local"]["timeout"] = json!(30_000);
        crate::write_settings(dir.path(),config.to_string()).unwrap();
        std::fs::write(config_path(dir.path(),"models.yml"),fixture_models(&provider)).unwrap();
        let mut pty = Pty::start_with_existing_files(dir.path());
        pty.ready();
        let mut pending = None;
        if phase != "idle" {
            pty.input("MCP exit\r");
            let (request,_) = pty.request(&provider);
            if phase == "tool" {
                calls_reply(request,json!([call("pending","mcp__local__echo",json!({"text":"hello"}))]));
                pty.text("mcp__local__echo");
            } else {
                pending = Some(request);
            }
        }
        pty.input("\u{3}");
        pty.finish();
        super::mcp::assert_stopped(dir.path());
        drop(pending);
    }
}

#[test]
fn mcp_tui_list_is_read_only_and_core_completion_updates_runtime() {
    let provider = server();
    let home = tempfile::tempdir().unwrap();
    crate::write_settings(home.path(), super::mcp::settings("").to_string()).unwrap();
    std::fs::write(config_path(home.path(), "models.yml"), fixture_models(&provider)).unwrap();
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready();
    pty.input("/mcp ");
    pty.text("reconnect");
    pty.text("reload");
    pty.input("list\r");
    pty.text("Configured MCP Servers");
    pty.text("connected");
    no_more(&provider);
    pty.input("\x1b");
    pty.ready();
    pty.input("/mcp disable local\r");
    pty.text("MCP state updated");
    pty.input("task without MCP\r");
    let (request, body) = receive(&provider);
    assert!(!body["tools"].to_string().contains("mcp__local__"));
    // 任务运行中提交管理命令必须本地拒绝，不能变成 steering。
    pty.input("/mcp enable local\r");
    pty.text("Finish or cancel the current task");
    final_reply(request, "task complete");
    pty.text("task complete");
    pty.ready();
    pty.input("\x01\x0b/mcp enable local\r");
    pty.text("MCP state updated");
    pty.input("task with MCP\r");
    let (request, body) = receive(&provider);
    assert!(body["tools"].to_string().contains("mcp__local__echo"));
    final_reply(request, "MCP reenabled");
    pty.text("MCP reenabled");
    pty.input("/quit\r");
    pty.finish();
    super::mcp::assert_stopped(home.path());
}
