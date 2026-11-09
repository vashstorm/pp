// Debug 构建的 `/test-ui`、`/test-read` 与 `/test-ask` PTY 回归：
// 真实 TUI 渲染内嵌 Responses SSE，Unavailable 启动也能运行，取消后保留草稿。

fn mock_tui_empty_start(home: &Path, project: &Path) -> Pty {
    let mut pty = Pty::start_with_args(project, home, &[]);
    pty.ready();
    pty
}

fn mock_tui_session_jsonl(home: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(home.join(".pp/agent/sessions")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
        .collect()
}

#[test]
fn tui_mock_ui_streams_incrementally_and_returns_to_unavailable() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = mock_tui_empty_start(home.path(), project.path());
    pty.resize(90, 120);
    pty.input("/test-ui\r");
    pty.wait_for("reasoning starts before assistant text", |screen| {
        screen.contents().contains("UI-TEST-THINKING-START")
    });
    pty.wait_for("early assistant marker is visible before the tail", |screen| {
        let text = screen.contents();
        text.contains("UI-TEST-ASSISTANT-START") && !text.contains("UI-TEST-ASSISTANT-END")
    });
    pty.text("UI-TEST-ASSISTANT-END");
    pty.text("This response comes from embedded Responses SSE data.");
    pty.text("UI-TEST-CODE-MARKER");
    let screen = pty.contents();
    assert_eq!(screen.matches("UI-TEST-ASSISTANT-START").count(), 1);
    assert_eq!(screen.matches("UI-TEST-CODE-MARKER").count(), 1);
    assert_eq!(screen.matches("UI-TEST-ASSISTANT-END").count(), 1);
    pty.text("Mock test complete");
    pty.ready();

    // 完成后必须回到原 Unavailable：普通任务被拒绝且草稿保持原样。
    pty.input("ordinary task\r");
    pty.draft("ordinary task");
    pty.input("\x01\x0b");
    pty.draft("");
    pty.input("\x03");
    pty.finish();
    assert!(mock_tui_session_jsonl(home.path()).is_empty());
}

#[test]
fn tui_mock_ui_escape_keeps_draft_and_a_rerun_starts_from_the_first_step() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = mock_tui_empty_start(home.path(), project.path());
    pty.resize(90, 120);
    pty.input("/test-ui\r");
    pty.wait_for("stream started before cancellation", |screen| {
        screen.contents().contains("UI-TEST-THINKING-START")
    });
    pty.input("keep this draft");
    pty.draft("keep this draft");
    pty.input("\x1b");
    pty.wait_for("cancelled mock run stays in the transcript", |screen| {
        let text = screen.contents();
        text.contains("Cancelled") && !text.contains("UI-TEST-ASSISTANT-END")
    });
    pty.draft("keep this draft");

    pty.input("\x01\x0b");
    pty.draft("");
    pty.input("/test-ui\r");
    pty.text("UI-TEST-ASSISTANT-END");
    pty.text("UI-TEST-CODE-MARKER");
    pty.ready();
    pty.input("\x03");
    pty.finish();
    assert!(mock_tui_session_jsonl(home.path()).is_empty());
}

#[test]
fn tui_mock_read_card_expands_and_collapses_real_content() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = mock_tui_empty_start(home.path(), project.path());
    pty.resize(60, 120);
    pty.input("/test-read\r");
    pty.text("Read fixture complete.");
    assert!(
        !pty.parser.screen().contents().contains("sixth-tail-marker"),
        "{}",
        pty.parser.screen().contents()
    );
    pty.input("\x0f");
    pty.text("sixth-tail-marker");
    pty.input("\x0f");
    pty.wait_for("read card collapses to its local summary", |screen| {
        !screen.contents().contains("sixth-tail-marker") && screen.contents().contains("main.rs")
    });
    pty.input("\x03");
    pty.finish();
    assert!(mock_tui_session_jsonl(home.path()).is_empty());
}

#[test]
fn tui_mock_ask_answers_three_questions_and_keeps_normalized_results() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = mock_tui_empty_start(home.path(), project.path());
    pty.resize(90, 120);
    pty.input("/test-ask\r");
    pty.wait_for("first ask question opens", |screen| {
        screen
            .contents()
            .contains("Pick the recommended local fixture option.")
    });
    pty.input("\x1b[B");
    pty.text("❯ 󰐽 Preview option");
    assert!(!pty.parser.screen().contents().contains("preview\nbody"));
    pty.input("\x1b[A\r");
    pty.text("Select several fixture labels.");
    pty.input(" \x1b[B \r");
    pty.text("Type a free-form fixture answer.");
    pty.input("custom fixture answer\r");
    pty.wait_for("ask review page is ready", |screen| {
        screen
            .contents()
            .contains("Enter submit · Tab questions · Esc cancel")
    });
    pty.input("\r");
    pty.text("Ask fixture complete.");
    pty.wait_for("normalized answers stay visible", |screen| {
        let text = screen.contents();
        text.contains("Recommended option")
            && text.contains("Alpha label")
            && text.contains("Beta label")
            && text.contains("custom fixture answer")
    });
    pty.ready();
    pty.input("\x03");
    pty.finish();
    assert!(mock_tui_session_jsonl(home.path()).is_empty());
}
