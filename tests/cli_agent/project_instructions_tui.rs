// 通过真实 PTY 验证启动规则加载、普通 read 展示、展开和恢复。

#[test]
fn agents_tui_resume_history_keeps_current_directory_rules_both_protocols() {
    let protocol = "response";
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let current = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("AGENTS.md"), "saved-directory-rule").unwrap();
    system_prompt::write(home.path(), ".pp/SYSTEM.md", "saved-source-only");
    system_prompt::write(current.path(), ".pp/SYSTEM_TEMPLATE.md", system_prompt::LIVE_TEMPLATE);
    system_prompt::write(current.path(), ".pp/APPEND_SYSTEM.md", "current-append-only");
    let cli = Cli::start_with(home.path(), Some("agents-saved-task"), fixture_settings(), &response_models(&fixture, Some(protocol)));
    let (request, _) = project_instructions::receive_for(&fixture, protocol);
    project_instructions::finish_request(request, protocol, "agents-saved-answer");
    assert!(cli.finish().status.success());
    let files = resume_history_session_files(home.path());
    let id = files[0].file_stem().unwrap().to_str().unwrap();
    std::fs::write(current.path().join("AGENTS.md"), "current-directory-rule").unwrap();
    let mut pty = Pty::start_with_args(current.path(), home.path(), &[]);
    pty.ready();
    system_prompt::write(current.path(), ".pp/SYSTEM_TEMPLATE.md", "{{#if broken}}");
    pty.input("/resume\r");
    // 等待后台扫描结束，避免在 Loading 状态发送会被忽略的方向键。
    pty.text("No sessions in current folder.");
    pty.input("\x1b[C");
    pty.text("agents-saved-task");
    pty.input("\r");
    resume_history_wait_resumed(&mut pty, id, &["agents-saved-task", "agents-saved-answer"]);
    pty.ready();
    std::fs::write(current.path().join("AGENTS.md"), "current-directory-rule-refreshed").unwrap();
    pty.input("agents-resumed-task\r");
    let (request, body) = pty.request_at(&fixture, api_path(protocol));
    system_prompt::assert_live(&body, protocol, "execute");
    let system = project_instructions::system(&body, protocol);
    assert!(system.contains("current-append-only"));
    assert!(!system.contains("saved-source-only"));
    assert_eq!(project_instructions::system(&body, protocol).matches("current-directory-rule-refreshed").count(), 1);
    assert!(!system.contains("saved-directory-rule"));
    assert!(body.to_string().contains("agents-saved-task"));
    project_instructions::finish_request(request, protocol, "agents-resume-done");
    pty.text("agents-resume-done");
    pty.ready();
    pty.input("\x03");
    pty.finish();
    no_more(&fixture);
}
