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

