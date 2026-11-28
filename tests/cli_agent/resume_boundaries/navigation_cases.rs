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
        pty.wait_for("a cross-directory resume loads the saved session", |screen| {
            input_is_ready(screen) && !screen.contents().contains("Resume session (")
                && screen.contents().contains("mcp project marker")
                && screen.contents().contains("mcp project answer")
        });
        pty.ready();
        // `/resume` only switches sessions: the launch directory's MCP connections stay in the same process and re-handshake is skipped.
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
        // Resume keeps the current directory's MCP connections; every process is reaped after exit.
        super::mcp::assert_stopped(project.path());
        super::mcp::assert_stopped(launch.path());
        drop(pending);
    }
}

