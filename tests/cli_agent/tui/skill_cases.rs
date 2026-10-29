use super::*;

#[test]
fn tui_skills_sources_preview_cancel_save_and_individual_disable() {
    fn selected_tab(screen: &vt100::Screen, label: &str) -> bool {
        let text = screen.contents();
        !text.contains("Skills · Working")
            && text.lines().enumerate().any(|(row, line)| {
                line.find(label)
                    .and_then(|offset| {
                        screen.cell(row as u16, line[..offset].chars().count() as u16)
                    })
                    .is_some_and(|cell| cell.bgcolor() == vt100::Color::Idx(5))
            })
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
            && text.lines().any(|line| {
                line.find("Sources")
                    .zip(line.find("Global management"))
                    .is_some_and(|(sources, global)| sources < global)
            })
    });
    pty.input("\x1b[C");
    pty.wait_for("右箭头选择全局管理页签", |screen| {
        selected_tab(screen, "Global management")
    });
    pty.text("[x] native");
    pty.input("\x1b[D");
    pty.wait_for("左箭头选择来源页签", |screen| {
        selected_tab(screen, "Sources")
    });
    pty.input("\x1b[Z");
    pty.wait_for("Shift+Tab 选择全局管理页签", |screen| {
        selected_tab(screen, "Global management")
    });
    pty.input(" \t");
    pty.wait_for("Tab 返回来源页签", |screen| {
        selected_tab(screen, "Sources")
    });
    pty.input(" ");
    pty.wait_for("来源预览完成：[x] claude", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources") && text.contains("[x] claude")
    });
    pty.input("\x1b[B ");
    pty.wait_for("来源预览完成：[x] codex", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources") && text.contains("[x] codex")
    });
    pty.input("\t");
    pty.text("[x] claude-skill");
    pty.text("[ ] native");
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
    pty.wait_for("来源预览完成：[x] claude", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources") && text.contains("[x] claude")
    });
    pty.input("\x1b[B ");
    pty.wait_for("来源预览完成：[x] codex", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources") && text.contains("[x] codex")
    });
    pty.input("\t\x1b[B ");
    pty.text("[ ] claude-skill");
    // 关闭再开启来源，仍保留尚未保存的单个技能选择。
    pty.input("\t\x1b[A ");
    pty.wait_for("来源预览完成：[ ] claude", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources") && text.contains("[ ] claude")
    });
    pty.input(" ");
    pty.wait_for("来源预览完成：[x] claude", |screen| {
        let text = screen.contents();
        selected_tab(screen, "Sources") && text.contains("[x] claude")
    });
    pty.input("\t");
    pty.text("[ ] claude-skill");
    std::fs::write(&settings_path, "invalid yaml").unwrap();
    pty.input("\r");
    pty.text("invalid YAML");
    pty.text("[ ] claude-skill");
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
    pty.input("\t");
    pty.text("[ ] claude-skill");
    pty.text("[x] codex-skill");
    pty.resize(4, 20);
    pty.wait_for("窄窗口显示紧凑页签", |screen| {
        selected_tab(screen, "G") && screen.contents().contains("S")
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

#[test]
fn tui_skills_manage_cancel_save_reload_restart_and_unavailable_models() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    super::super::skills::install(home.path(), "rust", "rust");
    let mut pty = Pty::start(&server, home.path());
    pty.ready();
    pty.input("/skills\r");
    pty.text("[ ] claude");
    pty.input("\t");
    pty.text("[x] rust");
    pty.input(" ");
    pty.text("[ ] rust");
    pty.input("\x1b");
    pty.ready();
    assert!(
        !std::fs::read_to_string(config_path(home.path(), "settings.yml"))
            .unwrap()
            .contains("disabled_skills")
    );
    pty.input("\r");
    pty.text("[ ] claude");
    pty.input("\t");
    pty.text("[x] rust");
    pty.input(" \r");
    pty.ready();
    let settings = std::fs::read_to_string(config_path(home.path(), "settings.yml")).unwrap();
    assert!(settings.contains("disabled_skills"));
    assert!(settings.contains("rust"));
    pty.input("\x03");
    pty.finish();
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready();
    pty.input("/skills\r");
    pty.text("[ ] claude");
    pty.input("\t");
    pty.text("[ ] rust");
    super::super::skills::install(home.path(), "second", "second");
    pty.input("r");
    pty.text("[ ] claude");
    pty.input("\t");
    pty.text("[x] second");
    std::fs::write(config_path(home.path(), "settings.yml"), "invalid yaml").unwrap();
    pty.input("\r");
    pty.text("invalid YAML");
    pty.text("Skills");
    assert_eq!(
        std::fs::read_to_string(config_path(home.path(), "settings.yml")).unwrap(),
        "invalid yaml"
    );
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        "{\"skill_dirs\":[]}",
    )
    .unwrap();
    pty.input("r");
    pty.text("[ ] claude");
    pty.input("\t");
    pty.text("No skills found");
    pty.resize(4, 20);
    pty.resize(30, 100);
    pty.text("Skills");
    pty.input("\x03");
    pty.finish();
    std::fs::write(config_path(home.path(), "settings.yml"), "{}").unwrap();
    std::fs::remove_file(config_path(home.path(), "models.yml")).unwrap();
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.text("models.yml");
    pty.input("/skills\r");
    pty.text("[ ] claude");
    pty.input("\t");
    pty.text("[x] rust");
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_skill_read_preview_busy_rejection_and_resize_for_both_apis() {
    for protocol in ["completion", "response"] {
        for preview in [true, false] {
            let server = server();
            let home = tempfile::tempdir().unwrap();
            super::super::skills::install(home.path(), "rust", "中文技能");
            std::fs::write(
                config_path(home.path(), "settings.yml"),
                if preview {
                    "read: {toolResultPreview: true}\n"
                } else {
                    "{}"
                },
            )
            .unwrap();
            std::fs::write(
                config_path(home.path(), "models.yml"),
                response_models(&server, Some(protocol)),
            )
            .unwrap();
            let mut pty = Pty::start_with_existing_files(home.path());
            pty.ready();
            pty.input("load skill\r");
            let path = api_path(protocol);
            let (request, _) = receive_at(&server, path);
            pty.input("/skills\r!");
            pty.draft("/skills!");
            pty.input("\x7f");
            pty.draft("/skills");
            assert!(input_is_ready(pty.parser.screen()));
            super::super::skills::send_skill_read(request, protocol, "中文技能");
            let (request, body) = receive_at(&server, path);
            assert!(body.to_string().contains("secret-skill-line-19"));
            pty.text("Read skill://");
            pty.text("name: 中文技能");
            pty.drain();
            let screen = pty.parser.screen();
            let text = screen.contents();
            assert!(
                !text.contains("skill ·"),
                "旧技能卡片标题不应再出现: {text}"
            );
            assert!(!text.contains("secret-skill-line-19"));
            assert!(
                text.contains("name: 中文技能"),
                "技能预览应包含 frontmatter: {text}"
            );
            pty.function_key('o');
            pty.text("secret-skill-line-19");
            pty.resize(4, 20);
            pty.resize(30, 100);
            pty.draft("/skills");
            if protocol == "response" {
                response_final(request, "skill loaded");
            } else {
                final_reply(request, "skill loaded");
            }
            pty.text("skill loaded");
            pty.input("\x03");
            pty.finish();
        }
    }
}
