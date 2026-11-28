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
    pty.wait_for("来源页签位于首位且默认选中", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources")
            && text
                .find("Sources")
                .zip(text.find("Global"))
                .is_some_and(|(sources, global)| sources < global)
    });
    pty.input("\t\x1b[B\t");
    pty.wait_for("切换全局管理页签", |screen| {
        selected_tab(screen, "Global management")
    });
    pty.text(&format!("{ENABLED} native"));
    pty.input(" \t\x1b[A\t");
    pty.wait_for("返回来源页签", |screen| {
        selected_tab(screen, "Sources")
    });
    pty.input(" ");
    pty.wait_for("来源预览完成：claude 已启用", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources") && text.contains(&format!("{ENABLED} claude"))
    });
    pty.input("\x1b[B ");
    pty.wait_for("来源预览完成：codex 已启用", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources") && text.contains(&format!("{ENABLED} codex"))
    });
    pty.input("\t\x1b[B\t");
    pty.text(&format!("{ENABLED} claude-skill"));
    pty.text(&format!("{DISABLED} native"));
    pty.input("\x1b");
    pty.ready();
    assert_eq!(std::fs::read_to_string(&settings_path).unwrap(), original);
    // 预览取消后，模型仍使用原目录和原逐项选择。
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
    pty.wait_for("选中来源页签", |screen| {
        selected_tab(screen, "Sources")
    });
    pty.input(" ");
    pty.wait_for("来源预览完成：claude 已启用", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources") && text.contains(&format!("{ENABLED} claude"))
    });
    pty.input("\x1b[B ");
    pty.wait_for("来源预览完成：codex 已启用", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources") && text.contains(&format!("{ENABLED} codex"))
    });
    pty.input("\t\x1b[B\t\x1b[B ");
    pty.text(&format!("{DISABLED} claude-skill"));
    // 关闭再开启来源，仍保留尚未保存的单个技能选择。
    pty.input("\t\x1b[A\t\x1b[A ");
    pty.wait_for("来源预览完成：claude 已停用", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources") && text.contains(&format!("{DISABLED} claude"))
    });
    pty.input(" ");
    pty.wait_for("来源预览完成：claude 已启用", |screen| {
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
    pty.wait_for("重开后默认显示来源", |screen| {
        selected_tab(screen, "Sources")
    });
    pty.input("\t\x1b[B\t");
    pty.text(&format!("{DISABLED} claude-skill"));
    pty.text(&format!("{ENABLED} codex-skill"));
    pty.resize(4, 20);
    pty.wait_for("窄窗口显示紧凑分类", |screen| {
        screen.contents().contains("Global")
    });
    pty.resize(30, 100);
    pty.wait_for("恢复完整页签", |screen| {
        selected_tab(screen, "Global management")
    });
    pty.text("Skills");
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}
