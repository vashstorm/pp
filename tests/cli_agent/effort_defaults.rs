fn effort_save_popup(screen: &vt100::Screen, message: &str, color: vt100::Color) -> bool {
    let (rows, cols) = screen.size();
    if rows < 5 || cols < 6 || !screen.contents().contains(message) {
        return false;
    }
    let Some(input_top) = (1..rows).find_map(|row| {
        (screen.cell(row, 0)?.contents() == "❯"
            && (0..cols).all(|col| {
                screen
                    .cell(row - 1, col)
                    .is_some_and(|cell| cell.contents() == "─")
            }))
        .then_some(row - 1)
    }) else {
        return false;
    };
    let Some(bottom) = input_top.checked_sub(1) else {
        return false;
    };
    let right = cols - 2;
    let border = |row, col, glyph| {
        screen
            .cell(row, col)
            .is_some_and(|cell| cell.contents() == glyph && cell.fgcolor() == color)
    };
    let Some(left) = (1..right).find(|&col| border(bottom, col, "╰")) else {
        return false;
    };
    let Some(top) = (0..bottom).find(|&row| border(row, right, "╮")) else {
        return false;
    };
    border(top, left, "╭")
        && border(bottom, right, "╯")
        && (left + 1..right).all(|col| border(bottom, col, "─"))
        && (top + 1..bottom).all(|row| {
            border(row, left, "│") && border(row, right, "│")
        })
        && (top..=bottom).all(|row| {
            screen
                .cell(row, cols - 1)
                .is_some_and(|cell| cell.contents().trim().is_empty())
        })
}

#[test]
fn tui_effort_ctrl_s_saves_selection_for_next_startup() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let path = config_path(home.path(), "settings.jsonc");
        let original = "{\n  // 保留其他设置与注释\n  \"think_level\": \"high\", // 等级说明\n  \"edit\": \"hashline\",\n  \"disabled_skills\": [\"uninstalled\"],\n}\n";
        std::fs::write(&path, original).unwrap();
        std::fs::write(
            config_path(home.path(), "models.jsonc"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_existing_files(home.path());
        pty.ready_effort("high");
        pty.input("/effort ");
        pty.text("Ctrl-S");
        pty.text("high（默认）");
        pty.input("\x1b[B\x13");
        pty.wait_for("绿色保存弹框紧邻输入框上方并靠右留一格", |screen| {
            effort_save_popup(
                screen,
                "默认思考等级已保存：xhigh",
                vt100::Color::Rgb(127, 166, 106),
            )
        });
        pty.ready_effort("high");
        pty.input("/effort ");
        pty.text("xhigh（默认）");
        assert!(!pty.parser.screen().contents().contains(" high（默认）"));
        assert!(
            pty.parser
                .screen()
                .contents()
                .contains("Deep thinking (current)")
        );
        pty.input("\x01\x0b");
        pty.draft("");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("// 保留其他设置与注释"));
        assert!(saved.contains("// 等级说明"));
        assert!(saved.contains("\"think_level\": \"xhigh\""));
        assert!(saved.contains("\"edit\": \"hashline\""));
        assert!(saved.contains("\"disabled_skills\": [\"uninstalled\"]"));
        no_more(&server);
        pty.input("\x03");
        pty.finish();

        let mut restarted = Pty::start_with_existing_files(home.path());
        restarted.ready_effort("xhigh");
        restarted.input("/effort ");
        restarted.text("xhigh（默认）");
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
        restarted.wait_for("重启后的绿色保存弹框颜色和位置", |screen| {
            effort_save_popup(
                screen,
                "默认思考等级已保存：no think",
                vt100::Color::Rgb(127, 166, 106),
            )
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
        config_path(home.path(), "models.jsonc"),
        fixture_models(&server),
    )
    .unwrap();
    let path = config_path(home.path(), "settings.jsonc");
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready_effort("no think");
    pty.input("/effort ");
    pty.text("no think（默认）");
    pty.input("\x01\x0b");
    pty.draft("");
    pty.input("/effort low");
    pty.text("Ctrl-S");
    pty.input("\x13");
    pty.wait_for("新建设置后的绿色保存弹框颜色和位置", |screen| {
        effort_save_popup(
            screen,
            "默认思考等级已保存：low",
            vt100::Color::Rgb(127, 166, 106),
        )
    });
    pty.ready_effort("no think");
    assert!(std::fs::read_to_string(&path).unwrap().contains("\"low\""));

    // 启动后配置变为无效内容时，保存应失败且不暴露原文。
    let invalid = "{ broken secret-marker-config }";
    std::fs::write(&path, invalid).unwrap();
    pty.input("/effort max");
    pty.text("Ctrl-S");
    pty.input("\x13");
    pty.wait_for("红色保存失败弹框紧邻输入框上方并靠右留一格", |screen| {
        effort_save_popup(
            screen,
            "保存默认思考等级失败",
            vt100::Color::Rgb(224, 108, 117),
        )
    });
    assert!(
        !pty.parser
            .screen()
            .contents()
            .contains("secret-marker-config")
    );
    pty.ready_effort("no think");
    pty.input("/effort ");
    pty.text("low（默认）");
    assert!(
        pty.parser
            .screen()
            .contents()
            .contains("Disable thinking (current)")
    );
    assert!(!pty.parser.screen().contents().contains("max（默认）"));
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
    json!({"providers":[slash_provider(server, "fixture", "completion", json!([
        {"id":"a","model":"model-a","thinking":{"efforts":["low","high"],"defaultLevel":"high"}},
        {"id":"b","model":"model-b","thinking":{"efforts":["medium","max"],"defaultLevel":"medium"}},
        {"id":"c","model":"model-c","thinking":{"efforts":["medium"]}}
    ]))]})
    .to_string()
}

fn model_thinking_home(server: &Server) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "settings.jsonc"),
        "{\n  \"think_level\": \"max\"\n}\n",
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.jsonc"),
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
    pty.text("high（默认）");
    let screen = pty.parser.screen().contents();
    assert!(screen.contains("Light thinking"));
    assert!(screen.contains("Deep thinking (current)"));
    for absent in [
        "Moderate thinking",
        "Extra-deep thinking",
        "Highest level the model supports",
        "Disable thinking",
        "low（默认）",
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
    pty.text("high（默认）");
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
    pty.text("medium（默认）");
    let screen = pty.parser.screen().contents();
    assert!(screen.contains("Moderate thinking (current)"));
    assert!(screen.contains("Highest level the model supports"));
    for absent in [
        "Light thinking",
        "Deep thinking",
        "Extra-deep thinking",
        "Disable thinking",
        "low（默认）",
        "high（默认）",
        "max（默认）",
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
    pty.text("medium（默认）");
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
    pty.text("medium（默认）");
    let screen = pty.parser.screen().contents();
    assert!(screen.contains("Moderate thinking (current)"));
    for absent in [
        "Light thinking",
        "Deep thinking",
        "Extra-deep thinking",
        "Highest level the model supports",
        "Disable thinking",
        "low（默认）",
        "max（默认）",
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
    let settings = config_path(home.path(), "settings.jsonc");
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready_effort("high");

    // 菜单内高亮 low 并 Ctrl-S：仅保存全局默认，会话档位保持 high。
    pty.input("/effort ");
    pty.text("high（默认）");
    pty.input("\x1b[A\x13");
    pty.wait_for("绿色保存弹框", |screen| {
        effort_save_popup(
            screen,
            "默认思考等级已保存：low",
            vt100::Color::Rgb(127, 166, 106),
        )
    });
    assert!(
        std::fs::read_to_string(&settings)
            .unwrap()
            .contains("\"think_level\": \"low\"")
    );
    pty.ready_effort("high");
    no_more(&server);

    // 重新打开菜单：显式模型默认标记仍在 high，不被全局保存覆盖。
    pty.input("/effort ");
    pty.text("high（默认）");
    let screen = pty.parser.screen().contents();
    assert!(screen.contains("high（默认）"));
    assert!(screen.contains("Deep thinking (current)"));
    assert!(!screen.contains("low（默认）"));
    assert!(!screen.contains("Light thinking (current)"));
    pty.input("\x01\x0b");
    pty.draft("");
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}
