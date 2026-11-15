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
    pty.text("mcp local echo");
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
                pty.text("mcp local echo");
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
    pty.input("/mcp ");
    pty.draft("/mcp");
    pty.text("reconnect");
    assert!(pty.contents().contains("Configured MCP Servers"));
    pty.input("\x1b[200~disable local\x1b[201~");
    pty.draft("/mcp disable local");
    assert!(pty.contents().contains("Configured MCP Servers"));
    pty.input("\r");
    pty.wait_for("MCP 管理完成并恢复输入", |screen| {
        let text = screen.contents();
        text.lines().any(|line| line.trim() == "❯")
            && !text.contains("Managing MCP servers…")
            && !text.contains("Configured MCP Servers")
    });
    pty.input("task without MCP\r");
    let (request, body) = receive(&provider);
    assert!(!body["tools"].to_string().contains("mcp__local__"));
    // 任务运行中提交管理命令必须本地拒绝，不能变成 steering。
    pty.input("/mcp enable local\r");
    pty.text("Finish or cancel the current task");
    final_reply(request, "task complete");
    pty.text("task complete");
    pty.ready();
    pty.input("\x01\x0b/mcp enable local");
    pty.draft("/mcp enable local");
    pty.input("\r");
    pty.wait_for("MCP 管理完成并恢复输入", |screen| {
        screen.contents().lines().any(|line| line.trim() == "❯")
            && !screen.contents().contains("Managing MCP servers…")
    });
    pty.input("task with MCP\r");
    let (request, body) = receive(&provider);
    assert!(body["tools"].to_string().contains("mcp__local__echo"));
    final_reply(request, "MCP reenabled");
    pty.text("MCP reenabled");
    pty.input("/quit\r");
    pty.finish();
    super::mcp::assert_stopped(home.path());
}

#[test]
fn mcp_tui_startup_tips_render_multiline_inside_welcome_area() {
    let provider = server();
    let home = tempfile::tempdir().unwrap();
    let mut config = super::mcp::settings("");
    config["mcpServers"]["broken"] = json!({"command": "/nonexistent/pp-mcp-fixture"});
    crate::write_settings(home.path(), config.to_string()).unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&provider),
    )
    .unwrap();
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready();
    pty.wait_for("多行 MCP 启动提示显示在欢迎页快捷入口下方", |screen| {
        let text = screen.contents();
        text.contains("Connected to MCP servers: 1 succeeded, 1 failed (")
            && text.contains("MCP: local connected (")
            && text.contains("✓")
            && text.contains("MCP: broken failed (")
            && text.contains("✗")
            && text.contains("starting MCP process failed")
    });
    let text = pty.contents();
    let lines: Vec<&str> = text.lines().collect();
    let hints_row = lines
        .iter()
        .position(|line| line.contains("/tips  All shortcuts"))
        .expect("welcome hints row");
    let broken_row = lines
        .iter()
        .position(|line| line.contains("MCP: broken failed ("))
        .expect("broken line");
    let reason_row = lines
        .iter()
        .position(|line| line.contains("starting MCP process failed"))
        .expect("broken reason line");
    let local_row = lines
        .iter()
        .position(|line| line.contains("MCP: local connected ("))
        .expect("local line");
    let summary_row = lines
        .iter()
        .position(|line| line.contains("Connected to MCP servers: 1 succeeded, 1 failed ("))
        .expect("summary line");
    let input_prompt_row = lines
        .iter()
        .position(|line| line.trim_start().starts_with('❯'))
        .expect("input prompt row");

    assert!(lines[local_row].trim_end().ends_with('✓'), "{text}");
    assert!(lines[broken_row].trim_end().ends_with('✗'), "{text}");
    assert_eq!(reason_row, broken_row + 1, "{text}");
    assert!(broken_row >= hints_row + 2, "{text}");
    assert!(local_row >= hints_row + 2, "{text}");
    assert_eq!(summary_row, reason_row.max(local_row) + 1, "{text}");
    // 输入栏上方不再保留 tip 行，总状态位于欢迎区内、输入上边框之上。
    assert!(summary_row + 1 < input_prompt_row, "{text}");
    assert_eq!(
        lines[hints_row].find("/tips"),
        lines[local_row].find("MCP:"),
        "{text}"
    );
    assert_eq!(
        lines[reason_row].find("starting MCP process failed"),
        lines[broken_row].find("MCP:").map(|x| x + 2),
        "{text}"
    );

    pty.input("hello\r");
    let (request, _) = receive(&provider);
    final_reply(request, "done");
    pty.text("done");
    pty.ready();
    let after_task = pty.contents();
    assert!(!after_task.contains("Connected to MCP servers:"), "{after_task}");
    assert!(!after_task.contains("MCP: local connected"), "{after_task}");
    pty.input("/quit\r");
    pty.finish();
    super::mcp::assert_stopped(home.path());
}
