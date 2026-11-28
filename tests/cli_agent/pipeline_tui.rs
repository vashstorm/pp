#[test]
fn pipeline_tui_shows_final_output_and_no_stage_panel() {
    let provider = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("input.txt"), "intermediate-only-marker\nvisible-line\n")
        .unwrap();
    let mut pty = bang_shell_start(&provider, home.path(), project.path(), "completion", "");

    pty.input("!{ cat input.txt; exit 7; } | tail -n 1\r");
    pty.wait_for("final output and the failing stage are visible", |screen| {
        let text = screen.contents();
        text.contains("visible-line") && text.contains("Non-zero pipeline stages: 1")
    });
    assert!(!pty.contents().contains("intermediate-only-marker"));

    pty.input("draft");
    pty.draft("draft");
    pty.input("\x1b[19~-kept");
    pty.draft("draft-kept");
    assert!(!pty.parser.screen().alternate_screen());
    pty.input("\x03");
    pty.finish();
    no_more(&provider);
}
