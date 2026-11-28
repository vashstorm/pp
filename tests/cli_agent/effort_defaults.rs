// 保存失败保留在消息区；不再依赖临时浮层。
fn model_wait_saved(pty: &mut Pty, home: &std::path::Path, reference: &str) {
    pty.wait_for("默认模型已提交并更新菜单标记", |screen| {
        read_agent_preferences(home).default_model.as_deref() == Some(reference)
            && screen.contents().contains(&format!("{reference} "))
    });
    assert!(!pty.parser.screen().contents().contains("Default model saved:"));
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

// 模型列表按收藏与使用量排序，顺序会随会话历史变化；从屏幕读取实际行序与
// 高亮项，让导航断言只依赖可见行为而不是某个固定排序。
const PICKER_MODELS: [&str; 2] = ["fixture/primary", "gateway/team/secondary"];

fn picker_row_order(screen: &vt100::Screen) -> Vec<&'static str> {
    let text = screen.contents();
    let mut rows: Vec<(usize, &'static str)> = PICKER_MODELS
        .into_iter()
        .filter_map(|model| {
            text.lines()
                .position(|line| line.contains(model))
                .map(|row| (row, model))
        })
        .collect();
    rows.sort_unstable();
    rows.into_iter().map(|(_, model)| model).collect()
}

fn picker_selected(screen: &vt100::Screen) -> Option<&'static str> {
    let text = screen.contents();
    PICKER_MODELS
        .into_iter()
        .find(|model| text.lines().any(|line| line.contains('❯') && line.contains(model)))
}

#[test]
fn sqlite_default_model_picker_keeps_draft_and_persists_across_restart() {
    let protocol = "response";
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
        model_wait_saved(&mut pty, home.path(), "gateway/team/secondary");
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
        restarted.completed();
        restarted.ready_model("gateway/team/secondary");
        restarted.input("草稿🙂");
        restarted.draft("草稿🙂");
        let cursor = restarted.parser.screen().cursor_position();
        restarted.function_key('p');
        restarted.text("fixture/primary");
        restarted.model_search("");
        restarted.input("fixture/primary");
        restarted.model_search("fixture/primary");
        restarted.input("\x13");
        model_wait_saved(&mut restarted, home.path(), "fixture/primary");
        restarted.ready_model("gateway/team/secondary");
        restarted.text("fixture/primary ");
        restarted.input("\x01\x0b");
        restarted.model_search("");
        restarted.input("\x1b[B");
        // Down 从当前模型移动到列表末尾；读取真实高亮模型再验证 Tab 补全。
        restarted.wait_for("模型列表高亮移动到末项", |screen| {
            picker_selected(screen) == picker_row_order(screen).last().copied()
        });
        let tabbed = picker_selected(restarted.parser.screen()).expect("highlighted model");
        restarted.input("\t");
        restarted.model_search(tabbed);
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
        model_wait_saved(&mut restarted, home.path(), "fixture/primary");
        assert_eq!(
            read_agent_preferences(home.path()).default_model.as_deref(),
            Some("fixture/primary")
        );
        restarted.input("\x03");
        restarted.finish();
        no_more(&server);
}

// 每模型 thinking 策略的真实 PTY 回归：A 仅 low/high 且显式默认 high，
// B 仅 medium/max 且显式默认 medium，C 单档 medium，全局 think_level 为 max。
fn model_thinking_models(server: &TestServer) -> String {
    json!({"providers":{"fixture": slash_provider(server, "fixture", "completion", json!({
        "a": {"model":"model-a","thinking":{"efforts":["low","high"],"defaultLevel":"high"}},
        "b": {"model":"model-b","thinking":{"efforts":["medium","max"],"defaultLevel":"medium"}},
        "c": {"model":"model-c","thinking":{"efforts":["medium"]}}}))}})
    .to_string()
}

fn model_thinking_home(server: &TestServer) -> tempfile::TempDir {
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

    // 本地拒绝不产生切档目标，草稿按新规则原样保留；清空后再输入普通任务。
    pty.draft("/effort low");
    pty.input("\u{1}");
    pty.input("\u{b}");
    pty.draft("");

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

