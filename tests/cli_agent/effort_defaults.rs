#[test]
fn tui_effort_ctrl_s_saves_selection_for_next_startup() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let path = config_path(home.path(), "settings.jsonc");
        let original = "{\n  // 保留其他设置与注释\n  \"think_level\": \"high\", // 等级说明\n  \"edit\": \"hashline\",\n  \"disabled_skills\": [\"uninstalled\"],\n}\n";
        std::fs::write(&path, original).unwrap();
        std::fs::write(
            config_path(home.path(), "models.jsonc"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_existing_files(home.path());
        pty.ready_effort("high");
        pty.input("/effort ");
        pty.text("Ctrl-S");
        pty.text("high（默认）");
        pty.input("\x1b[B\x13");
        pty.text("默认思考等级已保存：xhigh");
        pty.ready_effort("high");
        pty.input("/effort ");
        pty.text("xhigh（默认）");
        assert!(!pty.parser.screen().contents().contains(" high（默认）"));
        assert!(
            pty.parser
                .screen()
                .contents()
                .contains("Deep thinking (current)")
        );
        pty.input("\x01\x0b");
        pty.draft("");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("// 保留其他设置与注释"));
        assert!(saved.contains("// 等级说明"));
        assert!(saved.contains("\"think_level\": \"xhigh\""));
        assert!(saved.contains("\"edit\": \"hashline\""));
        assert!(saved.contains("\"disabled_skills\": [\"uninstalled\"]"));
        no_more(&server);
        pty.input("\x03");
        pty.finish();

        let mut restarted = Pty::start_with_existing_files(home.path());
        restarted.ready_effort("xhigh");
        restarted.input("/effort ");
        restarted.text("xhigh（默认）");
        restarted.input("\x01\x0b");
        restarted.draft("");
        restarted.input("verify saved default\r");
        let (request, body) = receive_at(&server, api_path(protocol));
        super::think::assert_level(&body, protocol, "xhigh");
        super::think::finish_request(request, protocol);
        restarted.ready_effort("xhigh");
        restarted.input("/effort n");
        restarted.text("Ctrl-S");
        restarted.input("\x13");
        restarted.text("默认思考等级已保存：no think");
        restarted.input("\x03");
        restarted.finish();
        let mut no_think = Pty::start_with_existing_files(home.path());
        no_think.ready_effort("no think");
        no_think.input("\x03");
        no_think.finish();
        no_more(&server);
    }
}

#[test]
fn tui_effort_ctrl_s_creates_missing_settings_and_reports_save_errors() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "models.jsonc"),
        fixture_models(&server),
    )
    .unwrap();
    let path = config_path(home.path(), "settings.jsonc");
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready_effort("no think");
    pty.input("/effort ");
    pty.text("no think（默认）");
    pty.input("\x01\x0b");
    pty.draft("");
    pty.input("/effort low");
    pty.text("Ctrl-S");
    pty.input("\x13");
    pty.text("默认思考等级已保存：low");
    assert!(std::fs::read_to_string(&path).unwrap().contains("\"low\""));

    // 启动后配置变为无效内容时，保存应失败且不暴露原文。
    let invalid = "{ broken secret-marker-config }";
    std::fs::write(&path, invalid).unwrap();
    pty.input("/effort max");
    pty.text("Ctrl-S");
    pty.input("\x13");
    pty.text("保存默认思考等级失败");
    pty.ready_effort("no think");
    pty.input("/effort ");
    pty.text("low（默认）");
    assert!(
        pty.parser
            .screen()
            .contents()
            .contains("Disable thinking (current)")
    );
    assert!(!pty.parser.screen().contents().contains("max（默认）"));
    pty.input("\x01\x0b");
    pty.draft("");
    assert!(
        !pty.parser
            .screen()
            .contents()
            .contains("secret-marker-config")
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
    pty.input("draft after failure");
    pty.draft("draft after failure");
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}
