use super::*;

#[test]
fn tui_skills_sources_preview_cancel_save_and_individual_disable() {
    const ENABLED: &str = "\u{f0765}";
    const DISABLED: &str = "\u{f0766}";
    fn selected_tab(screen: &vt100::Screen, label: &str) -> bool {
        let text = screen.contents();
        text.contains(&format!("Skills — {label}"))
            || text
                .lines()
                .any(|line| line.contains('❯') && line.contains(label))
    }
    let server = server();
    let home = tempfile::tempdir().unwrap();
    super::super::skills::install(home.path(), "native", "native");
    super::super::skills::install_source(home.path(), ".claude/skills", "claude-skill");
    super::super::skills::install_source(home.path(), ".agents/skills", "codex-skill");
    let mut pty = Pty::start(&server, home.path());
    pty.ready();
    let settings_path = config_path(home.path(), "settings.yml");
    let original = std::fs::read_to_string(&settings_path).unwrap();
    pty.input("/skills\r");
    pty.wait_for(
        "the Sources tab is first and selected by default",
        |screen| {
            let text = screen.contents();
            selected_tab(screen, "Sources")
                && text
                    .find("Sources")
                    .zip(text.find("Global"))
                    .is_some_and(|(sources, global)| sources < global)
        },
    );
    pty.input("\t\x1b[B\t");
    pty.wait_for("switching to the Global management tab", |screen| {
        selected_tab(screen, "Global management")
    });
    pty.text(&format!("{ENABLED} native"));
    pty.input(" \t\x1b[A\t");
    pty.wait_for("back on the Sources tab", |screen| {
        selected_tab(screen, "Sources")
    });
    pty.input(" ");
    pty.wait_for("the sources preview finishes: claude enabled", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources") && text.contains(&format!("{ENABLED} claude"))
    });
    pty.input("\x1b[B ");
    pty.wait_for("the sources preview finishes: codex enabled", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources") && text.contains(&format!("{ENABLED} codex"))
    });
    pty.input("\t\x1b[B\t");
    pty.text(&format!("{ENABLED} claude-skill"));
    pty.text(&format!("{DISABLED} native"));
    pty.input("\x1b");
    pty.ready();
    assert_eq!(std::fs::read_to_string(&settings_path).unwrap(), original);
    // After the preview is cancelled the model still uses the original directories and per-item selections.
    pty.input(&"\x7f".repeat("/skills".len()));
    pty.input("check cancelled preview\r");
    let (request, body) = pty.request(&server);
    let system = body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("native"));
    assert!(!system.contains("claude-skill"));
    assert!(!system.contains("codex-skill"));
    final_reply(request, "preview-cancelled");
    pty.text("preview-cancelled");
    pty.ready();
    pty.input("/skills sources\r");
    pty.wait_for("the Sources tab is selected", |screen| {
        selected_tab(screen, "Sources")
    });
    pty.input(" ");
    pty.wait_for("the sources preview finishes: claude enabled", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources") && text.contains(&format!("{ENABLED} claude"))
    });
    pty.input("\x1b[B ");
    pty.wait_for("the sources preview finishes: codex enabled", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources") && text.contains(&format!("{ENABLED} codex"))
    });
    pty.input("\t\x1b[B\t\x1b[B ");
    pty.text(&format!("{DISABLED} claude-skill"));
    // Closing and reopening sources still keeps the unsaved single-skill selection.
    pty.input("\t\x1b[A\t\x1b[A ");
    pty.wait_for("the sources preview finishes: claude disabled", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources") && text.contains(&format!("{DISABLED} claude"))
    });
    pty.input(" ");
    pty.wait_for("the sources preview finishes: claude enabled", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources") && text.contains(&format!("{ENABLED} claude"))
    });
    pty.input("\t\x1b[B\t");
    pty.text(&format!("{DISABLED} claude-skill"));
    std::fs::write(&settings_path, "invalid yaml").unwrap();
    pty.input("\r");
    pty.text("invalid YAML");
    pty.text(&format!("{DISABLED} claude-skill"));
    std::fs::write(&settings_path, &original).unwrap();
    pty.input("\r");
    pty.ready();
    pty.input(&"\x7f".repeat("/skills sources".len()));
    pty.input("check saved selection\r");
    let (request, body) = pty.request(&server);
    let system = body["messages"][0]["content"].as_str().unwrap();
    assert!(!system.contains("claude-skill"));
    assert!(system.contains("codex-skill"));
    final_reply(request, "selection-saved");
    pty.text("selection-saved");
    pty.ready();
    pty.input("\x03");
    pty.finish();
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready();
    pty.input("/skills\r");
    pty.wait_for("reopening shows Sources by default", |screen| {
        selected_tab(screen, "Sources")
    });
    pty.input("\t\x1b[B\t");
    pty.text(&format!("{DISABLED} claude-skill"));
    pty.text(&format!("{ENABLED} codex-skill"));
    pty.resize(4, 20);
    pty.wait_for("the narrow window shows compact categories", |screen| {
        screen.contents().contains("Global")
    });
    pty.resize(30, 100);
    pty.wait_for("the full tabs are back", |screen| {
        selected_tab(screen, "Global management")
    });
    pty.text("Skills");
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}
