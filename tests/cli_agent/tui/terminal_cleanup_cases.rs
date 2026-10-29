use super::*;

#[test]
fn tui_config_error_and_busy_quit_restore_terminal() {
    let server = server();
    let unavailable = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(unavailable.path(), "settings.yml"),
        fixture_settings(),
    )
    .unwrap();
    let mut pty = Pty::start_with_existing_files(unavailable.path());
    pty.wait_for(
        "配置错误显示为红色弹窗并保留欢迎页",
        |screen| {
            notification_bounds(screen, "Error", vt100::Color::Rgb(224, 108, 117)).is_some()
                && screen.contents().contains("models.yml")
                && screen.contents().contains("▄▄▄██▀▀██")
        },
    );
    let task = "create must-not-exist";
    pty.input(task);
    pty.input("\r");
    pty.text(task);
    no_more(&server);
    assert!(!unavailable.path().join("must-not-exist").exists());
    pty.input(&"\x7f".repeat(task.chars().count()));
    pty.input("/new\r");
    pty.text("/new");
    pty.text("models.yml");
    no_more(&server);
    assert!(!unavailable.path().join("must-not-exist").exists());
    pty.input("\x7f\x7f\x7f\x7f/quit\r");
    pty.wait_for("未创建会话的退出已恢复终端", |screen| {
        !screen.bracketed_paste()
    });
    assert!(!String::from_utf8_lossy(&pty.raw).contains("Resume with:"));
    pty.finish();
    no_more(&server);

    let busy = tempfile::tempdir().unwrap();
    let mut pty = Pty::start(&server, busy.path());
    pty.ready();
    pty.input("wait-for-reply\r");
    let (pending, body) = pty.request(&server);
    assert_eq!(body["messages"][1]["content"], "wait-for-reply");
    // Hold the actual HTTP request open: Ctrl-C must not join its worker.
    pty.input("\x03");
    pty.finish();
    drop(pending);
    no_more(&server);
}
