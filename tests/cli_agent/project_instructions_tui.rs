#[test]
fn agents_tui_init_submit_cancel_then_run_with_latest_rules_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(config_path(cwd.path(), "settings.yml"), fixture_settings()).unwrap();
        std::fs::write(config_path(cwd.path(), "models.yml"), response_models(&fixture, Some(protocol))).unwrap();
        let mut pty = Pty::start_with_existing_files(cwd.path());
        pty.ready();
        pty.input("/ini");
        pty.text("Create or update project AGENTS.md");
        pty.input("\t");
        pty.draft("/init");
        no_more(&fixture);
        pty.input("\r");
        let (request, body) = project_instructions::receive_for(&fixture, protocol);
        assert!(body.to_string().contains("Create or update AGENTS.md"));
        pty.text("/init");
        pty.input("\x1b");
        cancel_wait_cancelled_ready(&mut pty);
        drop(request);
        no_more(&fixture);
        std::fs::write(cwd.path().join("AGENTS.md"), "tui-latest-project-rule").unwrap();
        pty.input("after-init-cancellation\r");
        let (request, body) = project_instructions::receive_for(&fixture, protocol);
        assert!(project_instructions::system(&body, protocol).contains("tui-latest-project-rule"));
        project_instructions::finish_request(request, protocol, "agents-tui-done");
        pty.text("agents-tui-done");
        pty.ready();
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn agents_tui_resume_history_keeps_current_directory_rules_both_protocols() {
    for protocol in ["completion", "response"] {
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
        let (request, body) = project_instructions::receive_for(&fixture, protocol);
        system_prompt::assert_live(&body, protocol, "execute");
        let system = project_instructions::system(&body, protocol);
        assert!(system.contains("current-append-only"));
        assert!(!system.contains("saved-source-only"));
        assert_eq!(system.matches("current-directory-rule-refreshed").count(), 1);
        assert!(!system.contains("saved-directory-rule"));
        assert!(body.to_string().contains("agents-saved-task"));
        project_instructions::finish_request(request, protocol, "agents-resume-done");
        pty.text("agents-resume-done");
        pty.ready();
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}
