use super::*;

#[test]
fn tui_notification_error_scrolls_into_native_history_and_preserves_draft() {
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&fixture),
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        r#"{"notification_timeout_seconds":1}"#,
    )
    .unwrap();
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready();
    pty.input("/bogus\r");
    pty.wait_for("错误以完整红框进入消息区", |screen| {
        notification_bounds(screen, "Error", vt100::Color::Rgb(224, 108, 117)).is_some()
            && screen.contents().contains("unknown command; use /help")
    });
    pty.input("\x01\x0b草稿🙂");
    pty.draft("草稿🙂");
    let cursor = pty.parser.screen().cursor_position();
    let since = Instant::now();
    pty.wait_for_timeout(
        "错误超过旧过期时长仍保留",
        Duration::from_secs(4),
        |screen| {
            since.elapsed() >= Duration::from_millis(1100)
                && screen.contents().contains("unknown command; use /help")
        },
    );
    assert_eq!(pty.parser.screen().cursor_position(), cursor);
    pty.resize(12, 35);
    pty.draft("草稿🙂");
    pty.resize(30, 100);
    pty.wait_for("恢复窗口后错误仍完整显示", |screen| {
        notification_bounds(screen, "Error", vt100::Color::Rgb(224, 108, 117)).is_some()
    });
    pty.draft("草稿🙂");
    pty.input("\x03");
    pty.wait_for("退出后终端恢复", |screen| {
        !screen.bracketed_paste() && !screen.hide_cursor()
    });
    let (history, _) = pty.native_history();
    assert_eq!(history.matches("unknown command; use /help").count(), 1);
    assert!(!history.contains(" 1s "));
    pty.finish();
    no_more(&fixture);
}

#[test]
fn tui_notification_deprecated_settings_keep_startup_errors_and_redaction() {
    for (settings, expected) in [
        (
            r#"{"notification_timeout_seconds":2}"#,
            "Configuration unavailable",
        ),
        (
            r#"{"notification_show_countdown":false,"notification_timeout_seconds":2}"#,
            "Configuration unavailable",
        ),
        (
            r#"{"notification_timeout_seconds":"private-invalid-value"}"#,
            "invalid YAML settings",
        ),
    ] {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), settings).unwrap();
        let mut pty = Pty::start_with_existing_files(home.path());
        pty.wait_for(
            "启动错误使用消息区红框且不再有倒计时",
            |screen| {
                notification_bounds(screen, "Error", vt100::Color::Rgb(224, 108, 117)).is_some()
                    && screen.contents().contains(expected)
            },
        );
        let text = pty.parser.screen().contents();
        assert!(!text.contains("private-invalid-value"));
        assert!(!text.contains(" 2s ") && !text.contains(" 10s "));
        pty.input("草稿🙂");
        pty.draft("草稿🙂");
        pty.input("\x03");
        pty.finish();
    }
}

#[test]
fn tui_notification_startup_skill_warning_keeps_welcome_and_full_diagnostics() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        r#"{"skill_dirs":["./bad-root"]}"#,
    )
    .unwrap();
    std::fs::write(config_path(home.path(), "bad-root"), "not a directory").unwrap();
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready();
    pty.text("▄▄▄██▀▀██");
    assert!(
        !pty.parser
            .screen()
            .contents()
            .contains("Skill scan issues:")
    );
    assert!(
        !pty.parser
            .screen()
            .contents()
            .contains("Skill root path must be a directory")
    );
    pty.input("警告时草稿🙂");
    pty.draft("警告时草稿🙂");
    assert!(!pty.parser.screen().hide_cursor());
    assert!(pty.parser.screen().contents().contains("▄▄▄██▀▀██"));
    pty.input("\x01\x0b/skills\r");
    pty.text("Skills");
    pty.wait_for(
        "full skill root diagnostic across wrapped panel rows",
        |screen| {
            screen
                .contents()
                .chars()
                .filter(|ch| !ch.is_whitespace() && *ch != '│')
                .collect::<String>()
                .contains("Skillrootpathmustbeadirectory")
        },
    );
    pty.text("bad-root: Skill");
    assert!(
        !pty.parser
            .screen()
            .contents()
            .contains("Skill scan issues:")
    );
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_startup_centers_brand_and_keeps_footer_colors() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut pty = Pty::start(&server, dir.path());
    pty.ready();
    pty.text("Session & workflow");
    let screen = pty.parser.screen();
    let text = screen.contents();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines[2].trim(), "▄▄▄█    ██████    ██████");
    assert_eq!(lines[2].find('▄'), Some(44));
    assert_eq!(lines[5].trim(), "▀▀█████▀  ▄████      ██        ██");
    assert_eq!(
        lines[8].trim(),
        concat!("▀█▀       v", env!("CARGO_PKG_VERSION"))
    );
    assert!(lines[9].trim().is_empty());
    assert_eq!(tip_columns(screen, 10, 12..48).trim(), "Session & workflow");
    assert_eq!(tip_columns(screen, 10, 52..88).trim(), "Keyboard");
    assert_eq!(tip_columns(screen, 11, 12..48), "─".repeat(36));
    assert!(lines[17].contains("/tips  All shortcuts   ·   /help  Commands"));
    assert!(!text.contains("/compact"));
    assert!(!text.contains("/skills"));
    for (row, (left, right)) in [
        ("/model", "Enter"),
        ("/new", "Ctrl + J"),
        ("/resume", "Esc"),
        ("/plan", "Shift + Tab"),
    ]
    .into_iter()
    .enumerate()
    {
        let row = 12 + row as u16;
        assert_colored_tip_key(
            screen,
            row,
            12..48,
            left,
            vt100::Color::Rgb(122, 162, 247),
            12,
        );
        assert_colored_tip_key(
            screen,
            row,
            52..88,
            right,
            vt100::Color::Rgb(122, 162, 247),
            12,
        );
    }
    let context_start = lines[26].find(" Ctx ").expect("用量显示在输入栏上沿");
    assert!(!lines[26][context_start..].contains('/'));
    assert!(!lines[26][context_start..].contains('%'));
    assert!(lines[27].starts_with('❯'));
    assert_eq!(lines[28], "─".repeat(100));
    // 上沿右端显示用量，底部保留模型与思考级别。
    assert!(!lines[29].contains("Ctx "));
    assert!(lines[29].contains("fixture/primary"));
    assert!(lines[29].contains("off"));
    assert!(!text.contains("│ Note"));
    for (row, col, color) in [
        (2, 44, vt100::Color::Rgb(215, 175, 95)),
        (5, 41, vt100::Color::Rgb(122, 162, 247)),
        (2, 52, vt100::Color::Rgb(235, 235, 235)),
        (8, 52, vt100::Color::Idx(8)),
        (10, 12, vt100::Color::Rgb(215, 175, 95)),
        (11, 12, vt100::Color::Rgb(62, 68, 80)),
        (12, 12, vt100::Color::Rgb(122, 162, 247)),
        (12, 24, vt100::Color::Idx(7)),
        (17, 12, vt100::Color::Rgb(110, 117, 129)),
        (26, 0, vt100::Color::Rgb(127, 166, 106)),
        (29, 1, vt100::Color::Rgb(127, 166, 106)),
    ] {
        assert_eq!(screen.cell(row, col).unwrap().fgcolor(), color);
        assert_eq!(
            screen.cell(row, col).unwrap().bgcolor(),
            vt100::Color::Default
        );
    }

    pty.input("startup draft");
    pty.draft("startup draft");
    pty.resize(24, 80);
    pty.wait_for("80 列仍显示双列且输入位置固定", |screen| {
        let text = screen.contents();
        tip_columns(screen, 10, 2..38).trim() == "Session & workflow"
            && tip_columns(screen, 10, 42..78).trim() == "Keyboard"
            && text.contains("startup draft")
            && screen.cursor_position() == (21, 15)
    });
    pty.resize(30, 60);
    pty.wait_for(
        "窄屏提示切换为单列并保留八项入口",
        |screen| {
            let text = screen.contents();
            tip_columns(screen, 10, 2..58).trim() == "Session & workflow"
                && tip_columns(screen, 17, 2..58).trim() == "Keyboard"
                && text.contains("startup draft")
                && text.contains("/tips  All shortcuts")
        },
    );
    for (row, key) in [
        (12, "/model"),
        (13, "/new"),
        (14, "/resume"),
        (15, "/plan"),
        (19, "Enter"),
        (20, "Ctrl + J"),
        (21, "Esc"),
        (22, "Shift + Tab"),
    ] {
        assert_colored_tip_key(
            pty.parser.screen(),
            row,
            2..58,
            key,
            vt100::Color::Rgb(122, 162, 247),
            12,
        );
    }
    pty.resize(30, 38);
    pty.wait_for(
        "宽度不足时使用紧凑品牌并保留版本号",
        |screen| {
            let text = screen.contents();
            let mut lines = text.lines();
            lines.next().is_some_and(|line| line.trim() == "  PP")
                && lines
                    .next()
                    .is_some_and(|line| line.trim() == concat!("v", env!("CARGO_PKG_VERSION")))
                && text.contains("startup draft")
                && text.contains("Shift + Tab")
                && text.contains("/help  Commands")
        },
    );
    pty.resize(12, 40);
    pty.wait_for("小窗口显示紧凑欢迎页并保留草稿", |screen| {
        let text = screen.contents();
        text.lines().next().is_some_and(|line| line.trim() == "  PP")
                && text.contains("startup draft")
                && text.contains("/tips  All shortcuts")
                && text.contains("/help  Commands")
                && !text.contains("Session & workflow")
                && !text.contains("Keyboard")
                // 窄屏仍在输入栏上沿右端显示上下文字段。
                && text
                    .lines()
                    .nth(8)
                    .is_some_and(|line| line.contains("Ctx "))
    });
    let compact = pty.parser.screen();
    assert_eq!(
        compact.cell(0, 18).unwrap().fgcolor(),
        vt100::Color::Rgb(215, 175, 95)
    );
    assert_eq!(
        compact.cell(0, 21).unwrap().fgcolor(),
        vt100::Color::Rgb(235, 235, 235)
    );
    assert!(!compact.cell(0, 21).unwrap().bold());
    assert!(!compact.cell(0, 21).unwrap().italic());
    pty.resize(6, 20);
    pty.wait_for("极小窗口保留输入草稿和光标", |screen| {
        screen.contents().contains("startup draft") && screen.cursor_position() == (3, 15)
    });
    pty.resize(30, 100);
    pty.text("▄▄▄██▀▀██");
    pty.draft("startup draft");
    pty.input("\r");
    let (request, body) = pty.request(&server);
    assert_eq!(body["messages"][1]["content"], "startup draft");
    final_reply(request, "startup answer");
    pty.text("startup answer");
    pty.completed();
    assert!(!pty.parser.screen().contents().contains("▄▄▄██▀▀██"));
    pty.input("/new\r");
    pty.text("▄▄▄██▀▀██");
    assert!(!pty.parser.screen().contents().contains("Session reset."));
    pty.input("/help\r");
    pty.text("bash has no directory isolation");
    assert!(!pty.parser.screen().contents().contains("▄▄▄██▀▀██"));
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
}

const TIPS_ACCENT: vt100::Color = vt100::Color::Rgb(127, 166, 106);

// `/tips` 命令组等于命令注册表的可见项，按用户看到的键名忽略大小写排序。
const TIPS_COMMANDS: &[&str] = &[
    "/agents",
    "/changes",
    "/compact",
    "/effort",
    "/help",
    "/init",
    "/login",
    "/logout",
    "/mcp",
    "/model",
    "/new",
    "/plan",
    "/plan-list",
    "/quit",
    "/resume",
    "/Roles",
    "/skills",
    "/subagents",
    "/task-test",
    "/tips",
    "/tree",
];

// 快捷键组按显示键名递增；`⌥` 等非 ASCII 键名排在 ASCII 之后，两个 `Ctrl + S`
// 分别是模型与思考档位的默认值保存。
fn tips_shortcuts() -> Vec<&'static str> {
    if cfg!(target_os = "macos") {
        vec![
            "Ctrl + A",
            "Ctrl + C",
            "Ctrl + E",
            "Ctrl + F",
            "Ctrl + J",
            "Ctrl + K",
            "Ctrl + O",
            "Ctrl + S",
            "Enter",
            "Esc",
            "Esc Esc",
            "F1",
            "PgUp/PgDn",
            "Shift + Tab",
            "Tab",
            "↑ / ↓",
            "⌥ + Enter",
            "⌥ + P",
            "⌥ + PgUp/PgDn",
            "⌥ + Q",
            "⌥ + R",
            "⌥ + S",
            "⌥ + T",
        ]
    } else {
        vec![
            "Alt + Enter",
            "Ctrl + A",
            "Ctrl + C",
            "Ctrl + E",
            "Ctrl + F",
            "Ctrl + J",
            "Ctrl + K",
            "Ctrl + O",
            "Ctrl + P",
            "Ctrl + PgUp/PgDn",
            "Ctrl + Q",
            "Ctrl + R",
            "Ctrl + S",
            "Ctrl + S",
            "Ctrl + T",
            "Enter",
            "Esc",
            "Esc Esc",
            "F1",
            "PgUp/PgDn",
            "Shift + Tab",
            "Tab",
            "↑ / ↓",
        ]
    }
}

fn tips_commands() -> Vec<&'static str> {
    TIPS_COMMANDS
        .iter()
        .copied()
        .filter(|label| cfg!(debug_assertions) || *label != "/task-test")
        .collect()
}

// 键名对齐宽度：最长可见键名，下限 12 列。
fn tips_key_width() -> u16 {
    let widest = tips_commands()
        .into_iter()
        .chain(tips_shortcuts())
        .map(|label| label.chars().count())
        .max()
        .unwrap_or(0);
    u16::try_from(widest.max(12)).unwrap()
}

// 双列时右列的屏幕起始列：左侧两列边框，左列宽 `(正文宽 - 4) / 2`，列间四列空白。
fn tips_right_column(cols: u16) -> u16 {
    2 + (cols - 2 - 4) / 2 + 4
}

// 逐行读取指定键名列的键名，跳过标题行与空白行。
fn tips_visible_keys(screen: &vt100::Screen, column: u16, key_width: u16) -> Vec<String> {
    (0..screen.size().0)
        .filter(|&row| {
            screen
                .cell(row, 0)
                .is_some_and(|cell| cell.contents() == "│")
        })
        .filter_map(|row| {
            let key = tip_columns(screen, row, column..column + key_width);
            let key = key.trim().to_owned();
            (!key.is_empty() && !matches!(key.as_str(), "Tips:" | "Commands" | "Shortcuts"))
                .then_some(key)
        })
        .collect()
}

fn tips_key_row(screen: &vt100::Screen, column: u16, key_width: u16, label: &str) -> u16 {
    (0..screen.size().0)
        .find(|&row| {
            screen
                .cell(row, 0)
                .is_some_and(|cell| cell.contents() == "│")
                && tip_columns(screen, row, column..column + key_width).trim() == label
        })
        .unwrap_or_else(|| panic!("missing tips key {label}: {}", screen.contents()))
}

fn assert_tips_title(screen: &vt100::Screen, key_width: u16, title: &str, bold: bool) {
    let row = tips_key_row(screen, 2, key_width, title);
    for (offset, character) in title.chars().enumerate() {
        let col = 2 + u16::try_from(offset).unwrap();
        let cell = screen.cell(row, col).unwrap();
        assert_eq!(
            cell.contents(),
            character.to_string(),
            "row {row}, col {col}"
        );
        assert_eq!(cell.fgcolor(), TIPS_ACCENT, "row {row}, col {col}");
        assert_eq!(cell.bold(), bold, "row {row}, col {col}");
    }
}

#[test]
fn tui_tips_keeps_all_entries_and_leaves_active_jsonl_unchanged() {
    for protocol in ["completion", "response"] {
        let server = server();
        let (home, mut pty) = context_tui_start(&server, protocol, 100_000, 80);
        pty.text("Session & workflow");
        let startup = pty.parser.screen().contents();
        assert!(!startup.contains("/compact"));
        assert!(!startup.contains("/skills"));
        let commands = tips_commands();
        let shortcuts = tips_shortcuts();
        let key_width = tips_key_width();
        let key_field = key_width + 1;
        // 说明宽度预算为 20 列（见 `src/tips.rs`），颜色断言只需覆盖该区域。
        let span = key_field + 20;
        // 完整清单比首页精选长，先加高终端让两组整体留在可视区。
        pty.resize(60, 100);
        pty.input("/tips\r");
        pty.wait_for("本地提示展示完成", |screen| {
            let text = screen.contents();
            ["Tips:", "Commands", "Shortcuts", "/agents", "/tree"]
                .into_iter()
                .all(|expected| text.contains(expected))
        });
        let screen = pty.parser.screen();
        assert_tips_title(screen, key_width, "Tips:", false);
        assert_tips_title(screen, key_width, "Commands", true);
        assert_tips_title(screen, key_width, "Shortcuts", true);
        assert_colored_tip_key(
            screen,
            tips_key_row(screen, 2, key_width, "/agents"),
            2..2 + span,
            "/agents",
            TIPS_ACCENT,
            key_field,
        );
        // 100 列：正文 98 列，两个分组各自并成两列，左列在前半、右列在后半。
        let rows = commands.len().div_ceil(2);
        let shortcut_rows = shortcuts.len().div_ceil(2);
        let expected_left: Vec<String> = commands[..rows]
            .iter()
            .chain(&shortcuts[..shortcut_rows])
            .map(|label| (*label).to_owned())
            .collect();
        let expected_right: Vec<String> = commands[rows..]
            .iter()
            .chain(&shortcuts[shortcut_rows..])
            .map(|label| (*label).to_owned())
            .collect();
        let wide = tips_right_column(100);
        assert_eq!(tips_visible_keys(screen, 2, key_width), expected_left);
        assert_eq!(tips_visible_keys(screen, wide, key_width), expected_right);
        assert_eq!(
            tips_key_row(screen, 2, key_width, "/agents"),
            tips_key_row(screen, wide, key_width, commands[rows]),
            "同一物理行同时显示左右两列"
        );
        assert_colored_tip_key(
            screen,
            tips_key_row(screen, wide, key_width, commands[rows]),
            wide..wide + span,
            commands[rows],
            TIPS_ACCENT,
            key_field,
        );
        // 80 列：正文 78 列，仍双列且每行不折行。
        pty.resize(60, 80);
        let narrow = tips_right_column(80);
        let right_at_80 = expected_right.clone();
        pty.wait_for("80 列仍并排两列", move |screen| {
            tips_visible_keys(screen, narrow, key_width) == right_at_80
        });
        let screen = pty.parser.screen();
        assert_eq!(tips_visible_keys(screen, 2, key_width), expected_left);
        assert_colored_tip_key(
            screen,
            tips_key_row(screen, 2, key_width, commands[0]),
            2..2 + span,
            commands[0],
            TIPS_ACCENT,
            key_field,
        );
        // 60 列：正文 58 列装不下两列，按字母顺序输出完整单列。
        pty.resize(60, 60);
        let single: Vec<String> = commands
            .iter()
            .chain(&shortcuts)
            .map(|label| (*label).to_owned())
            .collect();
        pty.wait_for("60 列切换为完整单列", move |screen| {
            tips_visible_keys(screen, 2, key_width) == single
        });
        let screen = pty.parser.screen();
        let commands_title = tips_key_row(screen, 2, key_width, "Commands");
        let shortcuts_title = tips_key_row(screen, 2, key_width, "Shortcuts");
        assert!(commands_title < shortcuts_title);
        for label in commands.iter().chain(&shortcuts) {
            let row = tips_key_row(screen, 2, key_width, label);
            let in_group = if commands.contains(label) {
                commands_title < row && row < shortcuts_title
            } else {
                shortcuts_title < row
            };
            assert!(in_group, "{label} rendered outside its group");
            assert!(
                screen
                    .contents()
                    .lines()
                    .nth(usize::from(row))
                    .unwrap()
                    .starts_with("│ "),
                "{label} must be prefixed by the transcript border"
            );
        }
        assert_colored_tip_key(
            screen,
            tips_key_row(screen, 2, key_width, shortcuts[0]),
            2..2 + span,
            shortcuts[0],
            TIPS_ACCENT,
            key_field,
        );
        pty.resize(30, 100);
        assert!(!home.path().join(".pp/agent/sessions").exists());
        context_tui_exchange(
            &mut pty,
            &server,
            protocol,
            "tips-first-task",
            "tips-first-answer",
        );
        let files = resume_history_session_files(home.path());
        assert_eq!(files.len(), 1);
        let before = std::fs::read(&files[0]).unwrap();
        // 先确认命令草稿可见，再以清空输入作为本次命令的处理栅栏。
        pty.input("/tips");
        pty.draft("/tips");
        pty.input("\r");
        pty.text(&format!(
            "{:<width$}",
            shortcuts[shortcuts.len() - 1],
            width = usize::from(key_field)
        ));
        pty.draft("");
        assert_eq!(std::fs::read(&files[0]).unwrap(), before);
        pty.input("tips-next-task\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["tips-first-task", "tips-next-task"]
        );
        if protocol == "completion" {
            final_reply(request, "tips-next-answer");
        } else {
            response_final(request, "tips-next-answer");
        }
        pty.text("tips-next-answer");
        pty.completed();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn tui_notification_errors_stay_local_and_do_not_replay_after_resume() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), fixture_settings()).unwrap();
        let mut pty = Pty::start_with_existing_files(home.path());
        pty.ready();
        pty.input("/bogus\r");
        pty.text("unknown command; use /help");
        pty.input("\x01\x0bnotification failure task\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        request
            .respond(
                Response::from_string("private-provider-error fixture-key").with_status_code(400),
            )
            .unwrap();
        pty.wait_for(
            "任务失败显示红框且保留本地命令错误",
            |screen| {
                screen.contents().contains("HTTP status 400")
                    && screen.contents().contains("unknown command; use /help")
                    && notification_bounds(screen, "Error", vt100::Color::Rgb(224, 108, 117))
                        .is_some()
                    && input_is_ready(screen)
            },
        );
        assert!(
            !pty.parser
                .screen()
                .contents()
                .contains("private-provider-error")
        );
        pty.input("notification recovery task\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let sent = body.to_string();
        assert!(!sent.contains("unknown command; use /help"));
        assert!(!sent.contains("HTTP status 400"));
        plan_final(request, protocol, "notification recovery answer");
        pty.text("notification recovery answer");
        pty.completed();
        let id = saved_session_id(home.path());
        pty.input("\x03");
        pty.wait_for("退出后错误仅保留一次", |screen| {
            !screen.bracketed_paste() && !screen.hide_cursor()
        });
        let (history, _) = pty.native_history();
        assert_eq!(history.matches("unknown command; use /help").count(), 1);
        assert_eq!(history.matches("HTTP status 400").count(), 1);
        pty.finish();
        let journal = std::fs::read_to_string(
            home.path()
                .join(".pp/agent/sessions")
                .join(format!("{id}.jsonl")),
        )
        .unwrap();
        assert!(!journal.contains("unknown command; use /help"));
        // 既有 pp.error 失败元数据保持不变；没有新增本地消息卡片记录。
        let error_entries: Vec<Value> = journal
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|entry| entry.to_string().contains("HTTP status 400"))
            .collect();
        assert_eq!(error_entries.len(), 1);
        assert_eq!(error_entries[0]["customType"], "pp.error");
        let mut resumed = Pty::start_with_args(home.path(), home.path(), &["-r", &id]);
        resumed.text("notification recovery answer");
        resumed.ready();
        assert!(
            !resumed
                .parser
                .screen()
                .contents()
                .contains("unknown command; use /help")
        );
        assert!(
            !resumed
                .parser
                .screen()
                .contents()
                .contains("HTTP status 400")
        );
        resumed.input("\x03");
        resumed.finish();
        no_more(&fixture);
    }
}
