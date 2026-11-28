// TUI regression for `/test-task` in debug builds.

#[test]
fn tui_mock_task_shows_subagent_activity_and_returns_three_lane_markers() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = mock_tui_empty_start(home.path(), project.path());
    pty.resize(90, 120);
    pty.input("/test-task\r");
    pty.wait_for(
        "Task card and three subagent sessions appear while the batch runs",
        |screen| {
            let text = screen.contents();
            text.contains("\u{f0c0} 3")
                && !text.contains("Subagents:")
                && text
                    .lines()
                    .any(|line| line.contains("Task") && line.contains("Dispatched"))
                && text
                    .lines()
                    .any(|line| line.contains("fast-rust-count") && line.contains("Queued"))
        },
    );
    pty.text("completed (review-result-33)");
    pty.text("Mock test complete");
    let rendered = pty.contents().split_whitespace().collect::<Vec<_>>().join(" ");
    for summary in [
        "fast-rust-count: completed (fast-result-11)",
        "slow-symbol-lookup: completed (slow-result-22)",
        "review-events-risk: completed (review-result-33)",
    ] {
        assert!(rendered.contains(summary), "{rendered}");
    }
    pty.ready();
    pty.input("\x03");
    pty.finish();
    assert!(mock_tui_session_jsonl(home.path()).is_empty());
}
