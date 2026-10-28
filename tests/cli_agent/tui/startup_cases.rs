use super::*;

#[test]
fn tui_notification_fades_without_input_and_preserves_draft() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        r#"{"default_model":"fixture/primary","notification_timeout_seconds":2}"#,
    )
    .unwrap();
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready_effort("no think");
    pty.text("▄▄▄██▀▀██");
    let background: Vec<_> = (0..26)
        .map(|row| {
            (0..100)
                .map(|col| {
                    let cell = pty.parser.screen().cell(row, col).unwrap();
                    (cell.contents().to_owned(), cell.fgcolor(), cell.bgcolor())
                })
                .collect::<Vec<_>>()
        })
        .collect();

    pty.input("/bogus\r");
    let red = vt100::Color::Rgb(224, 108, 117);
    pty.wait_for(
        "full red error border above the input at the right edge",
        |screen| {
            notification_bounds(screen, "Error", red).is_some()
                && screen.contents().contains("unknown command; use /help")
        },
    );
    let (top, left, bottom, right) =
        notification_bounds(pty.parser.screen(), "Error", red).unwrap();
    assert_eq!(bottom, 25);
    assert_eq!(right, 98);
    pty.draft("/bogus");

    pty.input("\x01\x0b草稿🙂");
    pty.draft("草稿🙂");
    pty.resize(4, 20);
    pty.wait_for(
        "tiny terminal preserves the draft without an overlapping popup",
        |screen| {
            screen
                .contents()
                .lines()
                .any(|line| line.starts_with("❯ 草稿🙂"))
                && !screen.contents().contains("Error")
        },
    );
    pty.resize(30, 100);
    pty.ready_model("fixture/primary");
    pty.ready_effort("no think");
    pty.draft("草稿🙂");
    pty.wait_for(
        "active error notification survives restoring the terminal size",
        |screen| {
            notification_bounds(screen, "Error", red).is_some()
                && !screen.hide_cursor()
                && screen.cursor_position() == (27, 8)
        },
    );
    let cursor = pty.parser.screen().cursor_position();
    // 真实 SGR 鼠标事件进入弹框，顶边点击也只保持悬停。
    pty.input(&format!("\x1b[<35;{};{}M", right, top + 2));
    pty.input(&format!(
        "\x1b[<0;{};{}M\x1b[<0;{};{}m",
        right - 1,
        top + 1,
        right - 1,
        top + 1
    ));
    pty.wait_for(
        "悬停隐藏倒计时且不再显示复制按钮",
        |screen| {
            let text = screen.contents();
            notification_bounds(screen, "Error", red).is_some()
                && !text.contains(" 2s ")
                && !text.contains(" 1s ")
                && !text.contains("[Copy]")
                && !text.contains("[Sent]")
        },
    );
    pty.draft("草稿🙂");
    assert_eq!(pty.parser.screen().cursor_position(), cursor);
    let hovered_at = Instant::now();
    pty.wait_for_timeout(
        "鼠标停留超过配置时长后弹框仍然存在",
        Duration::from_secs(4),
        |screen| {
            hovered_at.elapsed() >= Duration::from_millis(2200)
                && !screen.contents().contains(" 2s ")
                && !screen.contents().contains(" 1s ")
                && notification_bounds(screen, "Error", red).is_some()
        },
    );
    pty.input("\x1b[<35;1;1M");
    // 移出后重新从两秒开始，无需后续输入即可更新倒计时并恢复背景。
    let left_at = Instant::now();
    pty.text(" 2s ");
    pty.text(" 1s ");
    pty.wait_for_timeout(
        "空闲错误弹框到期后恢复覆盖区域",
        Duration::from_secs(4),
        |screen| {
            !screen.contents().contains("Error")
                && !screen.contents().contains("unknown command; use /help")
                && (top..=bottom).all(|row| {
                    (left..=right).all(|col| {
                        let cell = screen.cell(row, col).unwrap();
                        let original = &background[usize::from(row)][usize::from(col)];
                        cell.contents().trim_end() == original.0.trim_end()
                            && (original.0.trim().is_empty() || cell.fgcolor() == original.1)
                            && cell.bgcolor() == original.2
                    })
                })
        },
    );
    assert!(left_at.elapsed() >= Duration::from_millis(1900));
    pty.draft("草稿🙂");
    assert!(!pty.parser.screen().hide_cursor());
    assert_eq!(pty.parser.screen().cursor_position(), cursor);
    pty.ready_effort("no think");

    pty.input("\x01\x0b/effort low");
    pty.text(if cfg!(target_os = "macos") {
        "⌥ + S"
    } else {
        "Ctrl + S"
    });
    pty.function_key('s');
    let green = vt100::Color::Rgb(127, 166, 106);
    pty.wait_for(
        "full green save notification above the input at the right edge",
        |screen| {
            notification_bounds(screen, "Note", green).is_some()
                && screen
                    .contents()
                    .contains("Default thinking effort saved: low")
        },
    );
    pty.ready_effort("no think");
    let saved = std::fs::read_to_string(config_path(home.path(), "settings.yml")).unwrap();
    assert!(saved.contains("think_level: low"));
    pty.input("草稿🙂");
    pty.draft("草稿🙂");
    let cursor = pty.parser.screen().cursor_position();
    pty.wait_for_timeout(
        "保存默认档位弹框无需输入即可到期消失",
        Duration::from_secs(4),
        |screen| {
            let text = screen.contents();
            !text.contains("Default thinking effort saved:")
                && !text.contains("Note")
                && !text.contains("│ Note")
                && text.contains("▄▄▄██▀▀██")
        },
    );
    pty.draft("草稿🙂");
    assert_eq!(pty.parser.screen().cursor_position(), cursor);
    assert!(!pty.parser.screen().hide_cursor());
    pty.ready_effort("no think");
    // 先观察完整命令，再等待 Reset ACK 清空输入，避免把 Ctrl-K 的中间空帧当作重置完成。
    pty.input("\x01\x0b/new");
    pty.draft("/new");
    pty.input("\r");
    pty.draft("");
    pty.input("/bogus\r");
    pty.wait_for("新会话保留两秒通知配置", |screen| {
        notification_bounds(screen, "Error", red).is_some() && screen.contents().contains(" 2s ")
    });
    pty.input("\x03");
    assert!(
        !pty.raw
            .windows(b"\x1b]52;".len())
            .any(|bytes| bytes == b"\x1b]52;")
    );
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_notification_settings_work_without_models_and_invalid_settings_use_defaults() {
    let red = vt100::Color::Rgb(224, 108, 117);
    for (settings, label) in [
        (r#"{"notification_timeout_seconds":2}"#, Some(" 2s ")),
        (
            r#"{"notification_show_countdown":false,"notification_timeout_seconds":2}"#,
            None,
        ),
        (
            r#"{"notification_timeout_seconds":"private-invalid-value"}"#,
            Some(" 10s "),
        ),
    ] {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), settings).unwrap();
        let mut pty = Pty::start_with_existing_files(home.path());
        pty.wait_for(
            "模型不可用时仍按通知设置显示启动错误",
            |screen| {
                let text = screen.contents();
                notification_bounds(screen, "Error", red).is_some()
                    && text.contains(if label == Some(" 10s ") {
                        "invalid YAML settings"
                    } else {
                        "Configuration unavailable"
                    })
                    && label.is_none_or(|label| text.contains(label))
            },
        );
        let text = pty.parser.screen().contents();
        assert!(!text.contains("[Copy]"));
        assert!(!text.contains("private-invalid-value"));
        if label.is_none() {
            assert!(!text.contains(" 2s "));
        }
        pty.input("草稿🙂");
        pty.draft("草稿🙂");
        if label != Some(" 10s ") {
            pty.wait_for("自定义时长无需输入即可到期", |screen| {
                !screen.contents().contains("Configuration unavailable")
            });
            pty.draft("草稿🙂");
            assert!(!pty.parser.screen().hide_cursor());
        }
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
    let yellow = vt100::Color::Rgb(229, 192, 92);
    pty.wait_for(
        "full yellow startup summary with welcome still visible",
        |screen| {
            let text = screen.contents();
            notification_bounds(screen, "Important", yellow).is_some()
                && text.contains("Skill scan issues: 1. Use /skills for details.")
                && text.contains("▄▄▄██▀▀██")
                && input_is_ready(screen)
        },
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
        assert_colored_tip_key(screen, row, 12..48, left, vt100::Color::Rgb(122, 162, 247));
        assert_colored_tip_key(screen, row, 52..88, right, vt100::Color::Rgb(122, 162, 247));
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

#[test]
fn tui_tips_keeps_all_entries_and_leaves_active_jsonl_unchanged() {
    for protocol in ["completion", "response"] {
        let server = server();
        let (home, mut pty) = context_tui_start(&server, protocol, 100_000, 80);
        pty.text("Session & workflow");
        let startup = pty.parser.screen().contents();
        assert!(!startup.contains("/compact"));
        assert!(!startup.contains("/skills"));
        let tips = [
            "/help list commands and usage",
            "Esc cancel the running task",
            "/model switch model, keep chat",
            "/new clear chat, keep model",
            "/resume reopen a saved session",
            "/plan plan, then approve edits",
            "/compact summarize older context",
            "/tree browse & rewind history",
            "/effort set model thinking level",
            "/init create/update AGENTS.md",
            "/skills enable or disable skills",
            "Shift + Tab cycle thinking levels",
            if cfg!(target_os = "macos") {
                "⌥ + P open the model picker"
            } else {
                "Ctrl + P open the model picker"
            },
            "F1 show commands and usage",
            "Ctrl + J insert a newline",
            "Ctrl + C quit pp",
        ];
        let model_key = if cfg!(target_os = "macos") {
            "⌥ + P"
        } else {
            "Ctrl + P"
        };
        let shortcut_keys = [
            (4, 12, "Shift + Tab"),
            (5, 13, model_key),
            (7, 15, "Ctrl + J"),
            (8, 16, "Ctrl + C"),
        ];
        pty.input("/tips\r");
        pty.wait_for("本地提示展示完成", |screen| {
            let text = screen.contents();
            let lines = text
                .lines()
                .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
                .collect::<Vec<_>>();
            !text.contains("▄▄▄██▀▀██")
                && tips
                    .iter()
                    .all(|tip| lines.iter().any(|line| line.contains(tip)))
        });
        let screen = pty.parser.screen();
        assert_eq!(
            screen.cell(1, 2).unwrap().fgcolor(),
            vt100::Color::Rgb(127, 166, 106)
        );
        assert!(screen.cell(1, 2).unwrap().bold());
        assert_eq!(screen.cell(1, 14).unwrap().fgcolor(), vt100::Color::Idx(7));
        for (wide_row, _, key) in shortcut_keys {
            // Transcript content is 98 columns wide, with its own two-column prefix.
            assert_tip_key(screen, wide_row, 53..100, key);
        }
        assert!(
            screen
                .contents()
                .lines()
                .nth(1)
                .unwrap()
                .contains("/effort     ")
        );
        pty.resize(30, 60);
        pty.wait_for("本地提示随窗口切换为完整单列", |screen| {
            let text = screen.contents();
            let lines = text
                .lines()
                .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
                .collect::<Vec<_>>();
            [(1, "/help"), (8, "/tree"), (16, "Ctrl + C")]
                .into_iter()
                .all(|(row, key)| {
                    text.lines()
                        .nth(row)
                        .is_some_and(|line| line.starts_with(&format!("│ {key:<12}")))
                })
                && tips
                    .iter()
                    .all(|tip| lines.iter().any(|line| line.contains(tip)))
        });
        for (_, narrow_row, key) in shortcut_keys {
            assert_tip_key(pty.parser.screen(), narrow_row, 2..60, key);
        }
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
        pty.text("/help       ");
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
