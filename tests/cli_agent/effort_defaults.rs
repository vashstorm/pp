// 等待条件是「保存结果可见且输入恢复」：弹框内容出现且输入行重新就绪，
// 不再断言颜色或几何位置。
fn effort_save_popup(screen: &vt100::Screen, message: &str) -> bool {
    screen.contents().contains(message) && input_is_ready(screen)
}

#[test]
fn tui_model_ctrl_s_keeps_picker_open_and_restores_draft_on_escape() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let path = config_path(home.path(), "settings.yml");
        let original = "# 保留默认模型之外的设置\nthink_level: high\nedit: hashline\ndefault_model: fixture/primary # 默认模型说明\n";
        std::fs::write(&path, original).unwrap();
        let models = json!({"providers":{
            "fixture": slash_provider(&server, "fixture", protocol, json!({
                "primary": {"model":"first-model"}})),
            "gateway": slash_provider(&server, "gateway", protocol, json!({
                "team/secondary": {"model":"second-model"}}))}}).to_string();
        let models_path = config_path(home.path(), "models.yml");
        std::fs::write(&models_path, &models).unwrap();
        let mut pty = Pty::start_with_existing_files(home.path());
        pty.ready_model("first-model");
        pty.input("/model ");
        pty.text("fixture/primary (default)");
        assert!(!pty.parser.screen().contents().contains("gateway/team/secondary (default)"));
        pty.input("\x01\x0b/model second");
        pty.text("gateway/team/secondary");
        pty.input("\x13");
        pty.text("Default model saved: gateway/team/secondary");
        pty.ready_model("first-model");
        pty.ready_effort("high");
        pty.draft("/model second");
        pty.text("gateway/team/secondary (default)");
        assert!(!pty.parser.screen().contents().contains("fixture/primary (default)"));
        pty.input("\x01\x0b");
        pty.draft("");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# 保留默认模型之外的设置"));
        assert!(saved.contains("# 默认模型说明"));
        assert!(saved.contains("think_level: high"));
        assert!(saved.contains("edit: hashline"));
        assert!(saved.contains("default_model: gateway/team/secondary"));
        assert_eq!(std::fs::read_to_string(&models_path).unwrap(), models);
        no_more(&server);
        pty.input("verify current model\r");
        let (request, body) = receive_at(&server, api_path(protocol));
        assert_eq!(body["model"], "first-model");
        super::think::assert_level(&body, protocol, "high");
        super::think::finish_request(request, protocol);
        pty.ready_model("first-model");
        pty.input("\x03");
        pty.finish();

        let mut restarted = Pty::start_with_existing_files(home.path());
        restarted.ready_model("second-model");
        restarted.input("/model ");
        restarted.text("gateway/team/secondary (default)");
        restarted.input("\x01\x0bverify startup model\r");
        let (request, body) = receive_at(&server, api_path(protocol));
        assert_eq!(body["model"], "second-model");
        super::think::finish_request(request, protocol);
        restarted.text("Session:");
        restarted.ready_model("second-model");
        restarted.input("草稿🙂");
        restarted.draft("草稿🙂");
        let cursor = restarted.parser.screen().cursor_position();
        restarted.input("\x10");
        restarted.text("fixture/primary");
        restarted.input("\x1b[A\x13");
        restarted.text("Default model saved: fixture/primary");
        restarted.ready_model("second-model");
        restarted.text("fixture/primary (default)");
        restarted.draft("/model");
        restarted.input("\t");
        restarted.draft("/model fixture/primary");
        restarted.input("\x1b");
        restarted.draft("草稿🙂");
        assert_eq!(restarted.parser.screen().cursor_position(), cursor);
        assert!(std::fs::read_to_string(&path).unwrap().contains("default_model: fixture/primary"));
        let invalid = "{ broken secret-marker-config }";
        std::fs::write(&path, invalid).unwrap();
        restarted.input("\x10");
        restarted.text("fixture/primary (default)");
        assert!(!restarted.parser.screen().contents().contains("gateway/team/secondary (default)"));
        restarted.input("\x13");
        restarted.text("Failed to save default model");
        restarted.draft("/model");
        restarted.text("fixture/primary (default)");
        restarted.input("\x1b");
        restarted.draft("草稿🙂");
        restarted.input("\x10");
        restarted.text("fixture/primary (default)");
        assert!(!restarted.parser.screen().contents().contains("gateway/team/secondary (default)"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
        restarted.input("\x03");
        restarted.finish();
        no_more(&server);
    }
}

#[test]
fn tui_model_ctrl_s_creates_missing_settings_and_preserves_invalid_settings() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(config_path(home.path(), "models.yml"), fixture_models(&server)).unwrap();
    let path = config_path(home.path(), "settings.yml");
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready_model("fixture-model");
    pty.input("/model\r");
    pty.text("fixture/primary (default)");
    pty.input("\x13");
    pty.text("Default model saved: fixture/primary");
    pty.ready_model("fixture-model");
    pty.draft("/model");
    pty.text("fixture/primary (default)");
    pty.input("\x01\x0b");
    pty.draft("");
    assert!(std::fs::read_to_string(&path).unwrap().contains("default_model: fixture/primary"));

    let invalid = "{ broken secret-marker-config }";
    std::fs::write(&path, invalid).unwrap();
    pty.input("draft after failure\x10");
    pty.text("fixture/primary");
    pty.input("\x13");
    pty.text("Failed to save default model");
    pty.ready_model("fixture-model");
    pty.draft("/model");
    pty.text("fixture/primary (default)");
    pty.input("\x1b");
    pty.draft("draft after failure");
    assert!(!pty.parser.screen().contents().contains("secret-marker-config"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_effort_ctrl_s_saves_selection_for_next_startup() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let path = config_path(home.path(), "settings.yml");
        let original = "# 保留其他设置与注释\nthink_level: high # 等级说明\nedit: hashline\ndisabled_skills: [uninstalled]\n";
        std::fs::write(&path, original).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_existing_files(home.path());
        pty.ready_effort("high");
        pty.input("/effort ");
        pty.text("Ctrl-S");
        pty.text("high (default)");
        pty.input("\x1b[B\x13");
        pty.wait_for("xhigh 保存结果可见且输入恢复", |screen| {
            effort_save_popup(screen, "Default thinking effort saved: xhigh")
        });
        pty.ready_effort("high");
        pty.input("/effort ");
        pty.text("xhigh (default)");
        assert!(!pty.parser.screen().contents().contains(" high (default)"));
        assert!(
            pty.parser
                .screen()
                .contents()
                .contains("Deep thinking (current)")
        );
        pty.input("\x01\x0b");
        pty.draft("");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# 保留其他设置与注释"));
        assert!(saved.contains("# 等级说明"));
        assert!(saved.contains("think_level: xhigh"));
        assert!(saved.contains("edit: hashline"));
        assert!(saved.contains("disabled_skills: [uninstalled]"));
        no_more(&server);
        pty.input("\x03");
        pty.finish();

        let mut restarted = Pty::start_with_existing_files(home.path());
        restarted.ready_effort("xhigh");
        restarted.input("/effort ");
        restarted.text("xhigh (default)");
        restarted.input("\x01\x0b");
        restarted.draft("");
        restarted.input("verify saved default\r");
        let (request, body) = receive_at(&server, api_path(protocol));
        super::think::assert_level(&body, protocol, "xhigh");
        super::think::finish_request(request, protocol);
        restarted.ready_effort("xhigh");
        restarted.input("/effort n");
        restarted.text("Ctrl-S");
        restarted.input("\x13");
        restarted.wait_for("重启后 no think 保存结果可见且输入恢复", |screen| {
            effort_save_popup(screen, "Default thinking effort saved: no think")
        });
        restarted.ready_effort("xhigh");
        restarted.input("\x03");
        restarted.finish();
        let mut no_think = Pty::start_with_existing_files(home.path());
        no_think.ready_effort("no think");
        no_think.input("\x03");
        no_think.finish();
        no_more(&server);
    }
}

#[test]
fn tui_effort_ctrl_s_creates_missing_settings_and_reports_save_errors() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();
    let path = config_path(home.path(), "settings.yml");
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready_effort("no think");
    pty.input("/effort ");
    pty.text("no think (default)");
    pty.input("\x01\x0b");
    pty.draft("");
    pty.input("/effort low");
    pty.text("Ctrl-S");
    pty.input("\x13");
    pty.wait_for("新建设置后 low 保存结果可见且输入恢复", |screen| {
        effort_save_popup(screen, "Default thinking effort saved: low")
    });
    pty.ready_effort("no think");
    assert!(std::fs::read_to_string(&path).unwrap().contains("think_level: low"));

    // 启动后配置变为无效内容时，保存应失败且不暴露原文。
    let invalid = "{ broken secret-marker-config }";
    std::fs::write(&path, invalid).unwrap();
    pty.input("/effort max");
    pty.text("Ctrl-S");
    pty.input("\x13");
    pty.wait_for("保存失败弹框可见且输入恢复", |screen| {
        effort_save_popup(screen, "Failed to save default thinking effort")
    });
    assert!(
        !pty.parser
            .screen()
            .contents()
            .contains("secret-marker-config")
    );
    pty.ready_effort("no think");
    pty.input("/effort ");
    pty.text("low (default)");
    assert!(
        pty.parser
            .screen()
            .contents()
            .contains("Disable thinking (current)")
    );
    assert!(!pty.parser.screen().contents().contains("max (default)"));
    pty.input("\x01\x0b");
    pty.draft("");
    assert!(
        !pty.parser
            .screen()
            .contents()
            .contains("secret-marker-config")
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
    pty.input("draft after failure");
    pty.draft("draft after failure");
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}

// 每模型 thinking 策略的真实 PTY 回归：A 仅 low/high 且显式默认 high，
// B 仅 medium/max 且显式默认 medium，C 单档 medium，全局 think_level 为 max。
fn model_thinking_models(server: &Server) -> String {
    json!({"providers":{"fixture": slash_provider(server, "fixture", "completion", json!({
        "a": {"model":"model-a","thinking":{"efforts":["low","high"],"defaultLevel":"high"}},
        "b": {"model":"model-b","thinking":{"efforts":["medium","max"],"defaultLevel":"medium"}},
        "c": {"model":"model-c","thinking":{"efforts":["medium"]}}}))}})
    .to_string()
}

fn model_thinking_home(server: &Server) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        "{\n  \"think_level\": \"max\"\n}\n",
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        model_thinking_models(server),
    )
    .unwrap();
    home
}

#[test]
fn model_thinking_tui_effort_menu_follows_model_policy_across_switches() {
    let server = server();
    let home = model_thinking_home(&server);
    let mut pty = Pty::start_with_existing_files(home.path());
    // 全局是 max，启动档位必须是模型 A 的显式默认 high。
    pty.ready_effort("high");

    // A 的菜单只列出 low/high，当前与默认标记都来自模型策略。
    pty.input("/effort ");
    pty.text("high (default)");
    let screen = pty.parser.screen().contents();
    assert!(screen.contains("Light thinking"));
    assert!(screen.contains("Deep thinking (current)"));
    for absent in [
        "Moderate thinking",
        "Extra-deep thinking",
        "Highest level the model supports",
        "Disable thinking",
        "low (default)",
        "Light thinking (current)",
    ] {
        assert!(!screen.contains(absent), "{absent} 不应出现在 A 的菜单");
    }
    pty.input("\x01\x0b");
    pty.draft("");

    // Shift-Tab 只在允许档位间往返，并保留中文草稿。
    pty.input("草稿🙂\x1b[Z");
    pty.ready_effort("low");
    pty.draft("草稿🙂");
    pty.input("\x1b[Z");
    pty.ready_effort("high");
    pty.draft("草稿🙂");

    // 发送草稿：请求档位与状态栏一致。
    pty.input("\r");
    let (request, body) = pty.request(&server);
    super::think::assert_level(&body, "completion", "high");
    assert_eq!(body["model"], "model-a");
    slash_assert_message(
        body["messages"].as_array().unwrap().last().unwrap(),
        "user",
        "草稿🙂",
    );
    super::think::finish_request(request, "completion");
    // Menus become actionable only after the worker's completion event;
    // an unchanged status label can still be visible while the task runs.
    pty.input("/effort ");
    pty.text("high (default)");
    pty.input("\x01\x0b");
    pty.draft("");
    pty.ready_effort("high");

    // Ctrl-P 切到 B：显式默认 medium 覆盖当前档位。
    pty.input("\x10");
    pty.text("fixture/b");
    pty.input("\x1b[B\t");
    pty.draft("/model fixture/b");
    pty.input("\r");
    pty.ready_model("model-b");
    pty.ready_effort("medium");

    // B 的菜单只出现 medium/max。
    pty.input("/effort ");
    pty.text("medium (default)");
    let screen = pty.parser.screen().contents();
    assert!(screen.contains("Moderate thinking (current)"));
    assert!(screen.contains("Highest level the model supports"));
    for absent in [
        "Light thinking",
        "Deep thinking",
        "Extra-deep thinking",
        "Disable thinking",
        "low (default)",
        "high (default)",
        "max (default)",
    ] {
        assert!(!screen.contains(absent), "{absent} 不应出现在 B 的菜单");
    }

    // 不支持的前缀不产生候选菜单。
    pty.input("l");
    pty.wait_for("不支持前缀没有候选菜单", |screen| {
        let text = screen.contents();
        text.contains("/effort l")
            && !text.contains("Moderate thinking")
            && !text.contains("Highest level the model supports")
            && !text.contains("Light thinking")
    });

    // 直接输入不支持的档位：本地错误弹框列出允许项，状态栏保持不变。
    pty.input("ow\r");
    pty.wait_for("直接输入不支持的档位报错", |screen| {
        notification_bounds(screen, "Error", vt100::Color::Rgb(224, 108, 117)).is_some()
            && screen
                .contents()
                .contains("thinking level is not supported by the current model")
            && screen.contents().contains("options: medium, max")
    });
    pty.ready_effort("medium");
    no_more(&server);

    // B 上的请求档位与状态栏一致，并保留 A 的历史。
    pty.input("任务B\r");
    let (request, body) = pty.request(&server);
    super::think::assert_level(&body, "completion", "medium");
    assert_eq!(body["model"], "model-b");
    let messages = body["messages"].as_array().unwrap();
    slash_assert_message(messages.last().unwrap(), "user", "任务B");
    assert!(
        messages
            .iter()
            .any(|message| message["content"] == json!("草稿🙂")),
        "切换模型后应保留之前的会话历史"
    );
    super::think::finish_request(request, "completion");
    pty.input("/effort ");
    pty.text("medium (default)");
    pty.input("\x01\x0b");
    pty.draft("");
    pty.ready_effort("medium");

    // 切回 A：显式默认 high 覆盖当前档位（而非回退到最低允许档 low）。
    pty.input("\x10");
    pty.text("fixture/a");
    pty.input("\x1b[A\t");
    pty.draft("/model fixture/a");
    pty.input("\r");
    pty.ready_model("model-a");
    pty.ready_effort("high");
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}

#[test]
fn model_thinking_tui_single_effort_shift_tab_retains_level_and_draft() {
    let server = server();
    let home = model_thinking_home(&server);
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready_effort("high");

    // 切到单档模型 C：当前 high 与全局 max 都不受支持，回退到唯一允许档 medium。
    pty.input("\x10");
    pty.text("fixture/c");
    pty.input("\x1b[B\x1b[B\t");
    pty.draft("/model fixture/c");
    pty.input("\r");
    pty.ready_model("model-c");
    pty.ready_effort("medium");

    // 单档时 Shift-Tab 保持档位与草稿，不触发错误弹框或模型请求。
    pty.input("单档草稿\x1b[Z!");
    pty.draft("单档草稿!");
    pty.ready_effort("medium");
    assert!(
        notification_bounds(pty.parser.screen(), "Error", vt100::Color::Rgb(224, 108, 117))
            .is_none(),
        "单档模型上 Shift-Tab 不应产生错误弹框"
    );
    no_more(&server);

    // C 的菜单只有 medium，同时是默认与当前档。
    pty.input("\x01\x0b");
    pty.draft("");
    pty.input("/effort ");
    pty.text("medium (default)");
    let screen = pty.parser.screen().contents();
    assert!(screen.contains("Moderate thinking (current)"));
    for absent in [
        "Light thinking",
        "Deep thinking",
        "Extra-deep thinking",
        "Highest level the model supports",
        "Disable thinking",
        "low (default)",
        "max (default)",
    ] {
        assert!(!screen.contains(absent), "{absent} 不应出现在 C 的菜单");
    }
    pty.input("\x01\x0b");
    pty.draft("");

    // 发送任务：请求档位与状态栏一致。
    pty.input("任务C\r");
    let (request, body) = pty.request(&server);
    super::think::assert_level(&body, "completion", "medium");
    assert_eq!(body["model"], "model-c");
    slash_assert_message(
        body["messages"].as_array().unwrap().last().unwrap(),
        "user",
        "任务C",
    );
    super::think::finish_request(request, "completion");
    pty.ready_effort("medium");
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}

#[test]
fn model_thinking_tui_ctrl_s_saves_global_without_overriding_model_default() {
    let server = server();
    let home = model_thinking_home(&server);
    let settings = config_path(home.path(), "settings.yml");
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready_effort("high");

    // 菜单内高亮 low 并 Ctrl-S：仅保存全局默认，会话档位保持 high。
    pty.input("/effort ");
    pty.text("high (default)");
    pty.input("\x1b[A\x13");
    pty.wait_for("low 全局默认保存结果可见且输入恢复", |screen| {
        effort_save_popup(screen, "Default thinking effort saved: low")
    });
    assert!(
        std::fs::read_to_string(&settings)
            .unwrap()
            .contains("think_level: low")
    );
    pty.ready_effort("high");
    no_more(&server);

    // 重新打开菜单：显式模型默认标记仍在 high，不被全局保存覆盖。
    pty.input("/effort ");
    pty.text("high (default)");
    let screen = pty.parser.screen().contents();
    assert!(screen.contains("high (default)"));
    assert!(screen.contains("Deep thinking (current)"));
    assert!(!screen.contains("low (default)"));
    assert!(!screen.contains("Light thinking (current)"));
    pty.input("\x01\x0b");
    pty.draft("");
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}
