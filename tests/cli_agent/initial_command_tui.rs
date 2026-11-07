// `pp --model … /COMMAND`：启动即提交一条文件命令，任务完成后会话仍可交互。
#[test]
fn initial_command_runs_on_startup_and_keeps_session_interactive() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(config_path(home.path(), "settings.yml"), fixture_settings()).unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();
    install_command(home.path(), "commit.md", "COMMIT-MARKER $1");
    let mut pty = Pty::start_with_args(
        home.path(),
        home.path(),
        &["--model", "fixture/primary", "/commit"],
    );

    // 启动命令与按 Enter 提交同一路径：请求里只能是展开后的模板正文。
    let (request, body) = pty.request(&server);
    assert_eq!(body["model"], "fixture-model");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    let prompt = slash_message_text(&messages[1]);
    assert!(prompt.contains("COMMIT-MARKER"), "{prompt}");
    final_reply(request, "COMMIT-DONE");
    pty.wait_for("启动命令的模板标记与回复", |screen| {
        let text = screen.contents();
        text.contains("COMMIT-MARKER") && text.contains("COMMIT-DONE")
    });
    // 命令跑完后输入框仍可用：会话没有随启动命令一起退出。
    pty.completed_after("COMMIT-DONE");
    assert!(input_is_ready(pty.parser.screen()));
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
}

// 未知启动命令只在界面提示并保留草稿，不发请求也不退出。
// 用例名带 `initial_command` 前缀：约定的定向筛选是 `test(initial_command)`（子串匹配）。
#[test]
fn initial_command_unknown_notifies_and_keeps_draft() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(config_path(home.path(), "settings.yml"), fixture_settings()).unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();
    let mut pty = Pty::start_with_args(home.path(), home.path(), &["/nope"]);
    pty.wait_for("未知启动命令的错误提示与输入焦点", |screen| {
        input_is_ready(screen)
            && screen.contents().contains("unknown command: /nope; use /help")
    });
    assert!(
        notification_bounds(
            pty.parser.screen(),
            "Error",
            vt100::Color::Rgb(224, 108, 117)
        )
        .is_some(),
        "未知命令应显示为错误条目\n{}",
        pty.contents()
    );
    pty.draft("/nope");
    assert_eq!(pty.prompt(), "/nope");
    no_more(&server);
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}
