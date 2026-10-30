// 等待条件是「保存结果可见且输入恢复」：弹框内容出现且输入行重新就绪，
// 不再断言颜色或几何位置。
fn effort_save_popup(screen: &vt100::Screen, message: &str) -> bool {
    screen.contents().contains(message) && input_is_ready(screen)
}

// 迁移完成后 agent.db 是偏好权威；用只读连接核对已提交的值，
// 运行期行为仍由真实 PTY 与重启证明，不断言 SQL 或 YAML 落点。
struct DbPreferences {
    default_model: Option<String>,
    think_level: String,
    favorites: Vec<String>,
}

fn read_agent_preferences(home: &std::path::Path) -> DbPreferences {
    let connection = rusqlite::Connection::open_with_flags(
        config_path(home, "agent.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let (default_model, think_level) = connection
        .query_row(
            "SELECT default_model, think_level FROM preferences WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let favorites = connection
        .prepare("SELECT model_reference FROM favorite_models ORDER BY position")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    DbPreferences {
        default_model,
        think_level,
        favorites,
    }
}

// 忙碌注入：外部写事务占住 agent.db 超过 250ms busy_timeout，保存必须失败。
fn hold_agent_db_write(home: &std::path::Path) -> rusqlite::Connection {
    let connection = rusqlite::Connection::open(config_path(home, "agent.db")).unwrap();
    connection.busy_timeout(std::time::Duration::from_secs(2)).unwrap();
    connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    connection
}

// 安全注入：过宽的数据库权限在打开时被拒绝，不回退也不改写文件。
fn restrict_agent_db(home: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        config_path(home, "agent.db"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
}

fn restore_agent_db(home: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        config_path(home, "agent.db"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
}

// 保存失败只显示固定领域提示：不得泄露路径、权限或 SQLite 细节。
fn assert_sanitized_database_error(screen: &vt100::Screen) {
    let contents = screen.contents();
    for leaked in ["agent.db", "denied", "os error", "SQL"] {
        assert!(
            !contents.contains(leaked),
            "{leaked} 不应出现在数据库错误提示中"
        );
    }
}

#[test]
fn sqlite_default_model_picker_keeps_draft_and_persists_across_restart() {
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
        pty.ready_model("fixture/primary");
        let migrated_settings = std::fs::read_to_string(&path).unwrap();
        pty.input("/model ");
        pty.text("fixture/primary  ");
        for (icon, color) in [
            ("", vt100::Color::Rgb(187, 154, 247)),
            ("", vt100::Color::Rgb(158, 206, 106)),
        ] {
            let screen = pty.parser.screen();
            let (rows, columns) = screen.size();
            assert!((0..rows).any(|row| (0..columns).any(|column| {
                screen.cell(row, column).is_some_and(|cell| {
                    cell.contents() == icon && cell.fgcolor() == color
                })
            })));
        }
        pty.text("Ctrl + F/Ctrl + S");
        pty.text(" favorite ·  default ·  current");
        assert!(!pty.parser.screen().contents().contains("gateway/team/secondary "));
        pty.input("\x01\x0bsecond");
        pty.text("gateway/team/secondary");
        pty.input("\x13");
        pty.text("Default model saved: gateway/team/secondary");
        pty.ready_model("fixture/primary");
        pty.ready_effort("high");
        pty.model_search("second");
        pty.text("gateway/team/secondary ");
        assert!(!pty.parser.screen().contents().contains("fixture/primary "));
        pty.input("\x1b");
        pty.draft("");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(saved, migrated_settings);
        assert!(saved.contains("# 保留默认模型之外的设置"));
        assert!(saved.contains("edit: hashline"));
        assert!(!saved.contains("default_model") && !saved.contains("think_level"));
        let preferences = read_agent_preferences(home.path());
        assert_eq!(
            preferences.default_model.as_deref(),
            Some("gateway/team/secondary")
        );
        // 旧 YAML 的 think_level: high 已随迁移成为数据库权威值。
        assert_eq!(preferences.think_level, "high");
        assert_eq!(std::fs::read_to_string(&models_path).unwrap(), models);
        no_more(&server);
        pty.input("verify current model\r");
        let (request, body) = receive_at(&server, api_path(protocol));
        assert_eq!(body["model"], "first-model");
        super::think::assert_level(&body, protocol, "high");
        super::think::finish_request(request, protocol);
        pty.ready_model("fixture/primary");
        pty.input("\x03");
        pty.finish();

        let mut restarted = Pty::start_with_existing_files(home.path());
        restarted.ready_model("gateway/team/secondary");
        restarted.input("/model ");
        restarted.text("gateway/team/secondary  ");
        restarted.input("\x1b");
        restarted.draft("");
        restarted.input("verify startup model\r");
        let (request, body) = receive_at(&server, api_path(protocol));
        assert_eq!(body["model"], "second-model");
        super::think::finish_request(request, protocol);
        restarted.text("Session:");
        restarted.ready_model("gateway/team/secondary");
        restarted.input("草稿🙂");
        restarted.draft("草稿🙂");
        let cursor = restarted.parser.screen().cursor_position();
        restarted.function_key('p');
        restarted.text("fixture/primary");
        restarted.input("\x1b[A");
        restarted.input("\x13");
        restarted.text("Default model saved: fixture/primary");
        restarted.ready_model("gateway/team/secondary");
        restarted.text("fixture/primary ");
        restarted.model_search("");
        restarted.input("\t");
        restarted.model_search("fixture/primary");
        restarted.input("\x1b");
        restarted.draft("草稿🙂");
        assert_eq!(restarted.parser.screen().cursor_position(), cursor);
        assert_eq!(
            read_agent_preferences(home.path()).default_model.as_deref(),
            Some("fixture/primary")
        );
        restrict_agent_db(home.path());
        restarted.function_key('p');
        restarted.text("fixture/primary ");
        assert!(!restarted.parser.screen().contents().contains("gateway/team/secondary "));
        restarted.input("\x13");
        restarted.text("Failed to save default model");
        assert_sanitized_database_error(restarted.parser.screen());
        restarted.model_search("");
        restarted.text("fixture/primary ");
        restarted.input("\x1b");
        restarted.draft("草稿🙂");
        restarted.function_key('p');
        restarted.text("fixture/primary ");
        assert!(!restarted.parser.screen().contents().contains("gateway/team/secondary "));
        // 保存失败没有改写 settings.yml，也没有留下部分提交。
        assert_eq!(std::fs::read_to_string(&path).unwrap(), migrated_settings);
        restore_agent_db(home.path());
        // 权限恢复后同一保存立即成功。
        restarted.input("fixture/primary");
        restarted.text("❯ fixture/primary");
        restarted.input("\x13");
        restarted.text("Default model saved: fixture/primary");
        assert_eq!(
            read_agent_preferences(home.path()).default_model.as_deref(),
            Some("fixture/primary")
        );
        restarted.input("\x03");
        restarted.finish();
        no_more(&server);
    }
}

// 缺少 settings.yml 时偏好只落 agent.db；忙碌库的保存失败不改变目录快照与草稿。
#[test]
fn sqlite_default_model_saves_without_settings_file_and_recovers_from_busy_database() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(config_path(home.path(), "models.yml"), fixture_models(&server)).unwrap();
    let path = config_path(home.path(), "settings.yml");
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready_model("fixture/primary");
    pty.input("/model\r");
    pty.text("fixture/primary ");
    pty.input("\x13");
    pty.text("Default model saved: fixture/primary");
    pty.ready_model("fixture/primary");
    pty.model_search("");
    pty.text("fixture/primary ");
    pty.input("\x1b");
    pty.draft("");
    assert!(!path.exists());
    assert_eq!(
        read_agent_preferences(home.path()).default_model.as_deref(),
        Some("fixture/primary")
    );

    let held = hold_agent_db_write(home.path());
    pty.input("draft after failure");
    pty.function_key('p');
    pty.text("fixture/primary");
    pty.input("\x13");
    pty.text("Failed to save default model");
    assert_sanitized_database_error(pty.parser.screen());
    pty.ready_model("fixture/primary");
    pty.model_search("");
    pty.text("fixture/primary ");
    pty.input("\x1b");
    pty.draft("draft after failure");
    drop(held);
    // 忙碌解除后同一保存立即恢复；失败没有留下部分提交或错误快照。
    pty.function_key('p');
    pty.text("fixture/primary ");
    pty.input("\x13");
    pty.text("Default model saved: fixture/primary");
    assert_eq!(
        read_agent_preferences(home.path()).default_model.as_deref(),
        Some("fixture/primary")
    );
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}

// 迁移完成后 settings.yml 不再承载偏好；运行期损坏它不得让偏好保存失败。
#[test]
fn sqlite_preference_save_ignores_invalid_settings_file() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(config_path(home.path(), "models.yml"), fixture_models(&server)).unwrap();
    let path = config_path(home.path(), "settings.yml");
    std::fs::write(&path, "edit: hashline\n").unwrap();
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready_model("fixture/primary");
    let invalid = "{ broken secret-marker-config }";
    std::fs::write(&path, invalid).unwrap();
    pty.input("/model\r");
    pty.text("fixture/primary ");
    pty.input("\x13");
    pty.text("Default model saved: fixture/primary");
    pty.ready_model("fixture/primary");
    pty.model_search("");
    pty.text("fixture/primary ");
    pty.input("\x1b");
    pty.draft("");
    // 损坏的 settings.yml 原样保留；偏好提交到 agent.db。
    assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
    assert_eq!(
        read_agent_preferences(home.path()).default_model.as_deref(),
        Some("fixture/primary")
    );
    assert!(!pty.parser.screen().contents().contains("secret-marker-config"));
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}

#[test]
fn sqlite_effort_selection_persists_across_restart() {
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
        let migrated_settings = std::fs::read_to_string(&path).unwrap();
        pty.input("/effort ");
        pty.text(if cfg!(target_os = "macos") {
            "⌥ + S"
        } else {
            "Ctrl + S"
        });
        pty.text("high (default)");
        pty.input("\x1b[B");
        pty.function_key('s');
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
        assert_eq!(saved, migrated_settings);
        assert!(saved.contains("# 保留其他设置与注释"));
        assert!(saved.contains("edit: hashline"));
        assert!(saved.contains("disabled_skills: [uninstalled]"));
        assert!(!saved.contains("think_level"));
        assert_eq!(read_agent_preferences(home.path()).think_level, "xhigh");
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
        restarted.text(if cfg!(target_os = "macos") {
            "⌥ + S"
        } else {
            "Ctrl + S"
        });
        restarted.function_key('s');
        restarted.wait_for("重启后 no think 保存结果可见且输入恢复", |screen| {
            effort_save_popup(screen, "Default thinking effort saved: no think")
        });
        assert_eq!(read_agent_preferences(home.path()).think_level, "none");
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

// 缺少 settings.yml 时 effort 只落 agent.db；忙碌库保存失败后目录快照与菜单不变。
#[test]
fn sqlite_effort_save_reports_busy_database_and_recovers() {
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
    pty.text(if cfg!(target_os = "macos") {
        "⌥ + S"
    } else {
        "Ctrl + S"
    });
    pty.function_key('s');
    pty.wait_for("新建数据库后 low 保存结果可见且输入恢复", |screen| {
        effort_save_popup(screen, "Default thinking effort saved: low")
    });
    pty.ready_effort("no think");
    assert!(!path.exists());
    assert_eq!(read_agent_preferences(home.path()).think_level, "low");

    // agent.db 忙碌超过 busy_timeout 时保存失败：固定领域提示，不暴露任何底层细节。
    let held = hold_agent_db_write(home.path());
    pty.input("/effort max");
    pty.text(if cfg!(target_os = "macos") {
        "⌥ + S"
    } else {
        "Ctrl + S"
    });
    pty.function_key('s');
    pty.wait_for("保存失败弹框可见且输入恢复", |screen| {
        effort_save_popup(screen, "Failed to save default thinking effort")
    });
    assert_sanitized_database_error(pty.parser.screen());
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
    drop(held);
    // 忙碌解除后同一保存立即成功，失败没有留下部分提交。
    pty.input("/effort max");
    pty.text(if cfg!(target_os = "macos") {
        "⌥ + S"
    } else {
        "Ctrl + S"
    });
    pty.function_key('s');
    pty.wait_for("忙碌解除后 max 保存结果可见且输入恢复", |screen| {
        effort_save_popup(screen, "Default thinking effort saved: max")
    });
    assert_eq!(read_agent_preferences(home.path()).think_level, "max");
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

    // 功能键 P 切到 B：显式默认 medium 覆盖当前档位。
    pty.function_key('p');
    pty.text("fixture/b");
    pty.input("\x1b[B\t");
    pty.model_search("fixture/b");
    pty.input("\r");
    pty.ready_model("fixture/b");
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
    pty.function_key('p');
    pty.text("fixture/a");
    pty.input("\x1b[A\t");
    pty.model_search("fixture/a");
    pty.input("\r");
    pty.ready_model("fixture/a");
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
    pty.function_key('p');
    pty.text("fixture/c");
    pty.input("\x1b[B\x1b[B\t");
    pty.model_search("fixture/c");
    pty.input("\r");
    pty.ready_model("fixture/c");
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
fn sqlite_global_effort_save_keeps_model_policy_default() {
    let server = server();
    let home = model_thinking_home(&server);
    let settings = config_path(home.path(), "settings.yml");
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready_effort("high");

    // 菜单内高亮 low 并按功能键 S：仅保存全局默认，会话档位保持 high。
    pty.input("/effort ");
    pty.text("high (default)");
    pty.input("\x1b[A");
    pty.function_key('s');
    pty.wait_for("low 全局默认保存结果可见且输入恢复", |screen| {
        effort_save_popup(screen, "Default thinking effort saved: low")
    });
    assert_eq!(read_agent_preferences(home.path()).think_level, "low");
    assert!(
        !std::fs::read_to_string(&settings)
            .unwrap()
            .contains("think_level")
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

#[test]
fn sqlite_favorites_search_persist_and_restore_drafts_without_requests() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let path = config_path(home.path(), "settings.yml");
        let original = "# 保留应用设置\nthink_level: none\nedit: hashline\n";
        std::fs::write(&path, original).unwrap();
        let models = json!({"providers":{
            "fixture": slash_provider(&server, "fixture", protocol, json!({
                "primary": {"model":"first-model"}})),
            "gateway": slash_provider(&server, "gateway", protocol, json!({
                "team/secondary": {"model":"second-model"}}))}}).to_string();
        let models_path = config_path(home.path(), "models.yml");
        std::fs::write(&models_path, &models).unwrap();
        let mut pty = Pty::start_with_existing_files(home.path());
        pty.ready_model("fixture/primary");
        let migrated_settings = std::fs::read_to_string(&path).unwrap();
        pty.input("草稿🙂");
        pty.draft("草稿🙂");
        let cursor = pty.parser.screen().cursor_position();
        pty.function_key('p');
        pty.text("Ctrl + F/Ctrl + S");
        pty.input("ECoNd");
        pty.text("gateway/team/secondary");
        pty.wait_for("包含搜索过滤其他模型", |screen| {
            !screen.contents().lines().any(|line| {
                line.trim_start_matches('❯')
                    .trim_start()
                    .starts_with("fixture/primary")
            })
        });
        pty.input("\x06");
        pty.text("gateway/team/secondary ");
        pty.wait_for("收藏星标显示为黄色", |screen| {
            let (rows, columns) = screen.size();
            (0..rows).any(|row| (0..columns).any(|column| {
                screen.cell(row, column).is_some_and(|cell| cell.contents() == "" && cell.fgcolor() == vt100::Color::Rgb(254, 188, 56))
            }))
        });
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(saved, migrated_settings);
        assert!(saved.contains("# 保留应用设置"));
        assert!(saved.contains("edit: hashline"));
        assert!(!saved.contains("favorite_models"));
        assert_eq!(
            read_agent_preferences(home.path()).favorites,
            ["gateway/team/secondary"]
        );
        pty.input("\x01\x0b");
        pty.wait_for("收藏模型排在最前", |screen| {
            let text = screen.contents();
            text.find("gateway/team/secondary ").zip(text.find("fixture/primary ")).is_some_and(|(favorite, other)| favorite < other)
        });
        pty.input("\x1b");
        pty.draft("草稿🙂");
        assert_eq!(pty.parser.screen().cursor_position(), cursor);
        no_more(&server);
        pty.input("\x03");
        pty.finish();

        let mut restarted = Pty::start_with_existing_files(home.path());
        restarted.ready_model("fixture/primary");
        restarted.function_key('p');
        restarted.text("gateway/team/secondary ");
        restarted.input("mIsSiNg");
        restarted.text("No matching models");
        restarted.input("\r\t\x06");
        restarted.model_search("mIsSiNg");
        no_more(&server);
        restarted.input("\x01\x0bWaY/TeAm");
        restarted.text("❯ gateway/team/secondary ");
        restarted.input("\x06");
        restarted.wait_for("取消收藏后移除星标", |screen| !screen.contents().contains("gateway/team/secondary "));
        assert!(read_agent_preferences(home.path()).favorites.is_empty());
        let held = hold_agent_db_write(home.path());
        restarted.input("\x06");
        restarted.text("Failed to save favorite models");
        assert_sanitized_database_error(restarted.parser.screen());
        assert!(!restarted.parser.screen().contents().contains("gateway/team/secondary "));
        drop(held);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), migrated_settings);
        assert_eq!(std::fs::read_to_string(&models_path).unwrap(), models);
        assert!(read_agent_preferences(home.path()).favorites.is_empty());
        no_more(&server);
        restarted.input("\r");
        restarted.ready_model("gateway/team/secondary");
        no_more(&server);
        restarted.input("verify selected model\r");
        let (request, body) = receive_at(&server, api_path(protocol));
        assert_eq!(body["model"], "second-model");
        super::think::finish_request(request, protocol);
        restarted.ready_model("gateway/team/secondary");
        restarted.input("\x03");
        restarted.finish();
        no_more(&server);
    }
}
