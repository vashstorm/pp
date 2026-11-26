#[test]
fn tui_slash_resume_mcp_servers_reap_after_cross_session_exit() {
    for phase in ["idle", "model", "tool"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let launch = tempfile::tempdir().unwrap();
        let mut settings = super::mcp::settings("call_timeout");
        settings["default_model"] = json!("fixture/primary");
        settings["notification_timeout_seconds"] = json!(1);
        settings["mcpServers"]["local"]["timeout"] = json!(30_000);
        resume_boundary_create_session_with(
            &server,
            home.path(),
            project.path(),
            &settings.to_string(),
            &fixture_models(&server),
            "mcp project marker",
            "mcp project answer",
        );

        let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
        pty.ready();
        resume_boundary_open(&mut pty);
        pty.text("mcp project marker");
        pty.input("\r");
        pty.wait_for("跨目录恢复保存会话", |screen| {
            input_is_ready(screen) && !screen.contents().contains("Resume session (")
                && screen.contents().contains("mcp project marker")
                && screen.contents().contains("mcp project answer")
        });
        pty.ready();
        // `/resume` 只切换会话：启动目录的 MCP 连接保持原进程，不重新握手。
        resume_boundary_assert_mcp_reused(launch.path());

        let mut pending = None;
        if phase != "idle" {
            pty.input("mcp phase\r");
            let (request, _) = pty.request(&server);
            if phase == "tool" {
                calls_reply(
                    request,
                    json!([call("pending", "mcp__local__echo", json!({"text":"hello"}))]),
                );
                pty.text("MCP local Echo");
            } else {
                pending = Some(request);
            }
        }
        pty.input("\u{3}");
        pty.finish();
        // 恢复沿用当前目录的 MCP 连接，退出后进程全部回收。
        super::mcp::assert_stopped(project.path());
        super::mcp::assert_stopped(launch.path());
        drop(pending);
    }
}

#[test]
fn tui_slash_resume_filters_current_folder_searches_and_deletes_saved_session() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let saved = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    let id = resume_boundary_create_session(&server, home.path(), saved.path(),
        "可删除的旧会话", "saved preview marker");
    let journal = resume_boundary_journal(home.path(), &id);
    let original = std::fs::read(&journal).unwrap();

    let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
    pty.ready();
    pty.input("current scope marker\r");
    let (request, _) = pty.request(&server);
    final_reply(request, "current scope answer");
    pty.completed_after("current scope answer");
    let current_id = resume_boundary_session_ids(home.path()).into_iter().find(|candidate| candidate != &id).unwrap();

    pty.input("/resume\r");
    pty.wait_for("当前目录仅显示当前会话和状态", |screen| {
        let text = screen.contents();
        text.contains("Resume session (current folder)") && text.contains("current scope marker")
            && text.contains("current · \u{f00c} done") && !text.contains("可删除的旧会话")
    });
    pty.input("\x1b[3~");
    pty.input("\r");
    pty.wait_for("当前会话不能删除且仍可选中关闭", |screen| {
        !screen.contents().contains("Resume session") && input_is_ready(screen)
    });
    assert!(resume_boundary_journal(home.path(), &current_id).exists());

    pty.input("/resume\r");
    pty.text("current · \u{f00c} done");
    pty.input("\x1b[C");
    pty.text("可删除的旧会话");
    pty.input("\x1b[200~PREVIEW marker\x1b[201~");
    pty.wait_for("按回答预览搜索，大小写不敏感且多个词均需命中", |screen| {
        let text = screen.contents();
        text.contains("> PREVIEW marker") && text.contains("可删除的旧会话")
            && !text.contains("current scope marker")
    });
    pty.input("\x1b[3~");
    pty.text("Delete selected session?");
    pty.input("\x1b");
    pty.wait_for("取消删除保留查询和条目", |screen| {
        let text = screen.contents();
        !text.contains("Delete selected session?") && text.contains("可删除的旧会话")
            && text.contains("> PREVIEW marker")
    });
    assert_eq!(std::fs::read(&journal).unwrap(), original);
    pty.input("\x1b[3~");
    pty.text("Delete selected session?");
    pty.input("y");
    pty.text("No matching sessions.");
    assert!(!journal.exists());
    no_more(&server);
    resume_boundary_close(&mut pty);
    pty.text("current scope answer");
    pty.input("still in current session\r");
    let (request, body) = pty.request(&server);
    assert!(body.to_string().contains("current scope marker"));
    final_reply(request, "current session preserved");
    pty.text("current session preserved");
    pty.ready();
    pty.input("/quit\r");
    pty.finish();
    assert_eq!(resume_boundary_session_ids(home.path()), [current_id]);
}
