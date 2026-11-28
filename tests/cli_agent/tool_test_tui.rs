// PTY regression for `/test-ui`, `/test-read` and `/test-ask` in debug builds:
// the real TUI renders the embedded Responses SSE, an Unavailable start still runs, and cancelling keeps the draft.

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
