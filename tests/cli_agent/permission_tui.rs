fn permission_fixture(home: &Path) {
    std::fs::write(
        config_path(home, "models.yml"),
        "providers:\n  fixture:\n    protocol: completion\n    api_key: fixture-key\n    base_url: http://127.0.0.1:9/v1\n    models:\n      primary:\n        model: fixture-model\n",
    )
    .unwrap();
}

fn permission_label(screen: &vt100::Screen, label: &str, color: vt100::Color) -> bool {
    let (rows, cols) = screen.size();
    let row = rows - 1;
    (0..cols).any(|column| {
        label.chars().enumerate().all(|(offset, ch)| {
            screen.cell(row, column + offset as u16).is_some_and(|cell| {
                cell.contents() == ch.to_string() && cell.fgcolor() == color
            })
        })
    })
}

#[test]
fn permission_tui_cycles_modes_with_both_shortcuts_and_keeps_draft() {
    use vt100::Color::Rgb;
    let home = tempfile::tempdir().unwrap();
    permission_fixture(home.path());
    let default_footer = |screen: &vt100::Screen| {
        let (rows, cols) = screen.size();
        permission_label(screen, "Accept-edits", Rgb(254, 188, 56))
            && screen
                .rows(0, cols)
                .nth(usize::from(rows - 1))
                .is_some_and(|row| row.contains("off · \u{f132} Accept-edits ·"))
    };
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.wait_for("默认启动时 effort 后带盾牌的 Accept-edits", default_footer);
    pty.input("permission draft");
    pty.draft("permission draft");
    pty.input("\x1b[D");
    let (row, column) = pty.parser.screen().cursor_position();
    pty.wait_for("草稿内光标", |screen| screen.cursor_position() == (row, column.saturating_sub(1)));
    let cursor = pty.parser.screen().cursor_position();
    for (keys, label, color) in [
        ("\x1b[17~", "YOLO", Rgb(224, 108, 117)),
        ("\x1bA", "Always-ask", Rgb(125, 207, 255)),
        ("\x1b[97;4u", "Accept-edits", Rgb(254, 188, 56)),
        ("\x1b[27;4;65~", "YOLO", Rgb(224, 108, 117)),
        ("\x1b[27;10;65~", "Always-ask", Rgb(125, 207, 255)),
        ("\x1b[65;3u", "Accept-edits", Rgb(254, 188, 56)),
    ] {
        pty.input(keys);
        pty.wait_for(&format!("{keys:?} 切换至 {label}"), |screen| {
            permission_label(screen, label, color)
        });
        assert_eq!(pty.prompt(), "permission draft");
        assert_eq!(pty.parser.screen().cursor_position(), cursor);
    }
    pty.resize(24, 17);
    pty.wait_for("窄屏 Accept-edits", |screen| {
        permission_label(screen, "Accept-edits", Rgb(254, 188, 56))
    });
    pty.input("\x1b[17~");
    pty.wait_for("窄屏 YOLO", |screen| {
        permission_label(screen, "YOLO", Rgb(224, 108, 117))
    });
    pty.input("\x03");
    pty.finish();

    let mut restarted = Pty::start_with_existing_files(home.path());
    restarted.wait_for("重启后恢复默认 Accept-edits", default_footer);
    restarted.input("\x03");
    restarted.finish();
}

#[test]
fn permission_tui_preserves_mode_across_new_and_plan() {
    let home = tempfile::tempdir().unwrap();
    permission_fixture(home.path());
    write_settings(home.path(), "tools: { permissionMode: always-ask }").unwrap();
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.wait_for("初始模式", |screen| screen.contents().contains("Always-ask"));
    pty.input("/permissions yolo\r");
    pty.wait_for("切换到新版 YOLO", |screen| {
        permission_label(screen, "YOLO", vt100::Color::Rgb(224, 108, 117))
    });
    pty.input("/new\r");
    pty.wait_for("新会话保留 YOLO 并清空权限详情", |screen| {
        permission_label(screen, "YOLO", vt100::Color::Rgb(224, 108, 117))
            && !screen.contents().contains("Permission mode: yolo")
    });
    pty.input("/plan\r");
    pty.wait_for("Plan 和 YOLO 标签", |screen| {
        let (rows, cols) = screen.size();
        permission_label(screen, "YOLO", vt100::Color::Rgb(224, 108, 117))
            && screen.rows(0, cols).nth(usize::from(rows - 1)).unwrap().contains("Plan")
    });
    pty.input("\x03");
    pty.finish();
}

#[test]
fn permission_tui_approves_and_saves_an_exact_bash_request() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let cwd = home.path().canonicalize().unwrap();
    let sandbox = pp_sandbox::Sandbox::new(
        pp_config::config::SandboxSettings {
            mode: pp_config::config::SandboxMode::WorkspaceWrite,
            network: false,
            ..Default::default()
        },
        &cwd,
    );
    if sandbox.policy().is_err() {
        // 平台上的隔离能力由 pp-sandbox 定向用例验证。
        return;
    }
    write_settings(home.path(), "tools: { permissionMode: accept-edits, sandbox: { network: false } }").unwrap();
    std::fs::write(config_path(home.path(), "models.yml"), fixture_models(&server)).unwrap();
    let mut pty = Pty::start_with_args(&cwd, home.path(), &[]);
    let command = "printf approved > marker";
    let request_arguments = json!({"command": command, "sandbox_permissions": {"network": true}});
    pty.input("request sandbox permission\r");
    let (request, body) = pty.request(&server);
    assert!(body["tools"].as_array().unwrap().iter().any(|tool| {
        tool["function"]["name"] == "bash"
            && tool["function"]["parameters"]["properties"].get("sandbox_permissions").is_some()
    }));
    calls_reply(request, json!([call("sandbox-test", "bash", request_arguments.clone())]));
    pty.wait_for("预执行授权与持久化选项", |screen| {
        let contents = screen.contents();
        contents.contains("outbound network") && contents.contains("Always allow in this project")
    });
    let marker = cwd.join("marker");
    let store = config_path(home.path(), "permission.toml");
    assert!(!marker.exists());
    assert!(!store.exists());
    pty.input("\x1b[B\x1b[B\r");
    let (request, body) = pty.request(&server);
    let result = body["messages"].as_array().unwrap().iter()
        .find(|message| message["role"] == "tool").unwrap();
    let output: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert_eq!(output["ok"], true, "{output}");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "approved");
    let rules = std::fs::read_to_string(&store).unwrap();
    assert!(rules.contains("version = 1"), "{rules}");
    assert!(rules.contains("[[rules]]"), "{rules}");
    assert!(rules.contains("[rules.grant]"), "{rules}");
    assert!(rules.contains(command), "{rules}");
    final_reply(request, "sandbox permission completed");
    pty.completed_after("sandbox permission completed");
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
}
