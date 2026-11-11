// Debug 构建的 `/test-fail`：真实 TUI 中的自动重试固定信息框与恢复。

#[test]
fn tui_run_test_fail_reports_retries_in_a_fixed_box_and_recovers() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = mock_tui_empty_start(home.path(), project.path());
    pty.resize(90, 120);
    pty.input("/test-fail\r");
    pty.wait_for("fixed retry box for the first attempt", |screen| {
        let text = screen.contents();
        text.contains("Retrying")
            && text.contains("attempt 1 failed: HTTP status 503")
            && text.contains("Esc cancels")
    });
    // 固定信息框不进入对话：界面上只有一处 `Retrying`。
    assert_eq!(pty.contents().matches("Retrying").count(), 1);
    pty.wait_for("second attempt reports the rate limit", |screen| {
        screen.contents().contains("attempt 2 failed: rate limited")
    });
    pty.text("Fail fixture complete");
    pty.text("Mock test complete");
    pty.wait_for("retry box disappears after recovery", |screen| {
        !screen.contents().contains("Retrying")
    });
    pty.ready();
    pty.input("\x03");
    pty.finish();
    assert!(mock_tui_session_jsonl(home.path()).is_empty());
}
