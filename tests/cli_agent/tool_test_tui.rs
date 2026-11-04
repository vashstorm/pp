fn tool_test_tui_start(server: &Server, home: &Path, project: &Path) -> Pty {
    crate::write_settings(
        home,
        r#"{"default_model":"fixture/primary","notification_timeout_seconds":1,"read":{"toolResultPreview":true}}"#,
    )
    .unwrap();
    std::fs::write(config_path(home, "models.yml"), fixture_models(server)).unwrap();
    Pty::start_with_args(project, home, &[])
}

fn tool_test_session_jsonl(home: &Path) -> Vec<std::path::PathBuf> {
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
fn tui_tool_read_card_expands_and_collapses_real_content() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = tool_test_tui_start(&server, home.path(), project.path());
    pty.ready();
    pty.resize(60, 120);
    pty.input("/test-read\r");
    pty.text("Local tool suite complete:");
    assert!(!pty.parser.screen().contents().contains("sixth-tail-marker"));
    pty.input("\x0f");
    pty.text("sixth-tail-marker");
    pty.input("\x0f");
    pty.wait_for("read card collapses to its local summary", |screen| {
        !screen.contents().contains("sixth-tail-marker")
            && screen.contents().contains("Ctrl + O to expand")
    });
    assert!(!pty.parser.screen().contents().contains("/test-read arguments"));
    pty.input("\x03");
    pty.finish();
    assert!(tool_test_session_jsonl(home.path()).is_empty());
    no_more(&server);
}

#[test]
fn tui_tool_test_all_suites_three_question_ask_and_completion_note() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = tool_test_tui_start(&server, home.path(), project.path());
    pty.ready();
    pty.resize(90, 120);
    pty.input("/test-tools\r");
    pty.wait_for("ask panel opens as the last tool fixture", |screen| {
        screen.contents().contains("Pick the recommended local fixture option.")
    });
    // 等待时只显示交互面板，预览正文在提交后的问答卡片中保留。
    pty.input("\x1b[B");
    pty.text("❯ 󰐽 Preview option");
    assert!(!pty.parser.screen().contents().contains("preview"));
    pty.input("\x1b[A\r");
    pty.text("Select several fixture labels.");
    pty.input(" \x1b[B \r");
    pty.text("Type a free-form fixture answer.");
    pty.input("custom fixture answer\r");
    pty.wait_for("ask review page is ready", |screen| {
        screen.contents().contains("Enter submit · Tab questions · Esc cancel")
    });
    pty.input("\r");
    pty.text("Local tool suite complete:");
    pty.text("preview");
    pty.wait_for("normalized answers and completion note stay visible", |screen| {
        let text = screen.contents();
        text.contains("Recommended option")
            && text.contains("Alpha label")
            && text.contains("Beta label")
            && text.contains("custom fixture answer")
    });
    pty.ready();
    pty.input("\x03");
    pty.finish();
    assert!(tool_test_session_jsonl(home.path()).is_empty());
    no_more(&server);
}

#[test]
fn tui_tool_test_escape_cancels_and_recovers_without_fixture_leftovers() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = tool_test_tui_start(&server, home.path(), project.path());
    pty.ready();
    pty.resize(80, 120);
    pty.input("/test-tools\r");
    pty.wait_for("ask panel is open before cancellation", |screen| {
        screen.contents().contains("Pick the recommended local fixture option.")
    });
    pty.input("\x1b");
    pty.wait_for("cancelled tool suite returns to Ready", |screen| {
        let text = screen.contents();
        text.contains("Task cancelled.") && !text.contains("Local tool suite complete")
    });
    pty.ready();
    pty.input("/test-read\r");
    pty.text("Local tool suite complete:");
    assert!(
        std::fs::read_dir(project.path())
            .unwrap()
            .flatten()
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".pp-test-tools-"))
    );
    pty.input("\x03");
    pty.finish();
    assert!(tool_test_session_jsonl(home.path()).is_empty());
    no_more(&server);
}

#[test]
fn tui_ui_test_streams_progressively_completes_once_and_cancels() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = tool_test_tui_start(&server, home.path(), project.path());
    pty.ready();
    pty.resize(90, 120);
    pty.input("/test-ui\r");
    pty.wait_for("early assistant marker is visible before the tail", |screen| {
        let text = screen.contents();
        text.contains("UI-TEST-ASSISTANT-START") && !text.contains("UI-TEST-ASSISTANT-END")
    });
    pty.text("UI-TEST-ASSISTANT-END");
    pty.text("Mock UI stream complete:");
    let screen = pty.parser.screen().contents();
    assert_eq!(screen.matches("UI-TEST-ASSISTANT-START").count(), 1);
    assert_eq!(screen.matches("UI-TEST-CODE-MARKER").count(), 1);
    assert_eq!(screen.matches("UI-TEST-ASSISTANT-END").count(), 1);
    assert!(screen.contains("UI-TEST-THINKING-START"));
    pty.ready();
    pty.input("\x03");
    pty.finish();
    assert!(tool_test_session_jsonl(home.path()).is_empty());
    no_more(&server);

    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = tool_test_tui_start(&server, home.path(), project.path());
    pty.ready();
    pty.resize(60, 120);
    pty.input("/test-ui\r");
    pty.text("UI-TEST-ASSISTANT-START");
    pty.input("\x1b");
    pty.wait_for("cancelled mock UI returns to Ready", |screen| {
        let text = screen.contents();
        text.contains("Cancelled")
            && text.contains("UI-TEST-ASSISTANT-START")
            && !text.contains("Mock UI stream complete")
    });
    pty.ready();
    pty.input("\x03");
    pty.finish();
    assert!(tool_test_session_jsonl(home.path()).is_empty());
    no_more(&server);
}
