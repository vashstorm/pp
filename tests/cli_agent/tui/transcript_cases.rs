use super::*;

#[test]
fn tui_activity_is_single_live_emphasized_and_cleared_by_new() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut pty = Pty::start(&server, dir.path());
    pty.ready();
    for turn in 1..=2 {
        pty.input(&format!("task {turn}\r"));
        let (request, _) = pty.request(&server);
        pty.text("✻ Requesting model ·");
        assert_eq!(
            pty.parser
                .screen()
                .contents()
                .matches("Completed ·")
                .count(),
            0
        );
        pty.wait_for("等待网络时秒表持续更新", |screen| {
            let text = screen.contents();
            text.contains("✻ Requesting model · 1s") && text.matches('✻').count() == 1
        });
        final_reply(request, &format!("answer {turn}"));
        pty.wait_for("每轮完成提示与明亮加粗样式", |screen| {
            let text = screen.contents();
            text.matches('✻').count() == 1
                && text.contains("✻ Completed ·")
                && text.contains(" · done ")
                && label_color(screen, "✻ Completed ·") == vt100::Color::Rgb(190, 198, 212)
                && (0..30).any(|row| {
                    screen
                        .cell(row, 4)
                        .is_some_and(|cell| cell.contents() == "C" && cell.bold())
                })
                && input_is_ready(screen)
        });
        let text = pty.parser.screen().contents();
        assert!(text.find(&format!("answer {turn}")).unwrap() < text.find('✻').unwrap());
    }
    pty.input("/new\r");
    pty.wait_for("新会话清空完成提示", |screen| {
        input_is_ready(screen) && !screen.contents().contains("Completed ·")
    });
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_exit_keeps_conversation_on_normal_screen() {
    for exit in ["\x03", "/quit\r"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut pty = Pty::start(&server, dir.path());
        pty.ready();
        pty.input("保留提问\r");
        let (request, _) = pty.request(&server);
        final_reply(request, "retained-answer 中😀\n    indented code");
        pty.text("retained-answer");
        pty.ready();
        if exit == "\x03" {
            pty.input("unsubmitted-draft");
            pty.draft("unsubmitted-draft");
        } else {
            pty.input("/quit");
            pty.draft("/quit");
        }
        let exit_start = pty.raw.len();
        pty.input(if exit == "\x03" { exit } else { "\r" });
        pty.wait_for("退出已恢复输入模式", |screen| {
            !screen.bracketed_paste()
        });
        let output = &pty.raw[exit_start..];
        let text = String::from_utf8_lossy(output);
        assert!(
            !text.contains("retained-answer"),
            "退出不重放已有正文：{text:?}"
        );
        assert!(!text.contains("\x1b[2J") && !text.contains("\x1b[3J"));
        pty.wait_for("退出后普通屏幕保留完整对话", |screen| {
            let text = screen.contents();
            !screen.alternate_screen()
                && text.contains("保留提问")
                && text.contains("retained-answer 中😀")
                && text.contains("indented code")
                && text.contains("✻ Completed ·")
                && text.contains("fixture/primary")
                && text.contains('❯')
                && background_color(screen, "保留提问") == USER_BACKGROUND
                && background_color(screen, "retained-answer") == ASSISTANT_BACKGROUND
        });
        let text = pty.parser.screen().contents();
        if exit == "\x03" {
            assert!(text.contains("unsubmitted-draft"), "{text}");
        }
        assert_eq!(
            background_color(pty.parser.screen(), "保留提问"),
            USER_BACKGROUND
        );
        assert_eq!(
            background_color(pty.parser.screen(), "retained-answer"),
            ASSISTANT_BACKGROUND
        );
        assert!(!text.contains("You"), "{text}");
        assert!(!text.contains("Assistant"), "{text}");
        assert!(text.lines().any(|line| line.starts_with('─')), "{text}");
        assert_eq!(text.matches("retained-answer").count(), 1, "{text}");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn tui_exit_keeps_scrolled_history_and_footer_styles() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut pty = Pty::start(&server, dir.path());
    pty.ready();
    pty.input("long-history\r");
    let (request, _) = pty.request(&server);
    let answer = (0..90)
        .map(|index| format!("    retained-{index:03} 中😀"))
        .collect::<Vec<_>>()
        .join("\n");
    final_reply(request, &answer);
    pty.text("retained-089");
    pty.ready();
    pty.input("draft-中\x1b[5~");
    pty.draft("draft-中");
    pty.wait_for("退出前已向上翻页", |screen| {
        !screen.contents().contains("retained-089")
    });
    pty.input("\x03");
    pty.wait_for("退出后保留输入框和状态栏", |screen| {
        !screen.alternate_screen()
            && screen.contents().contains("❯ draft-中")
            && screen.contents().contains("fixture/primary")
    });
    pty.wait_for("退出模式恢复", |screen| !screen.bracketed_paste());
    let (_, raw) = pty.native_history();
    let mut parser = vt100::Parser::new(600, 100, 0);
    parser.process(&raw);
    let screen = parser.screen();
    let text = screen.contents();
    assert!(text.contains("long-history"), "{text}");
    assert!(text.contains("❯ draft-中"), "{text}");
    let border_row = text.lines().position(|line| line.starts_with('─')).unwrap() as u16;
    assert_eq!(
        screen.cell(border_row, 0).unwrap().fgcolor(),
        vt100::Color::Rgb(127, 166, 106)
    );
    assert_eq!(background_color(screen, "long-history"), USER_BACKGROUND);
    assert_eq!(
        background_color(screen, "retained-000"),
        ASSISTANT_BACKGROUND
    );
    parser.process(b"shell> ");
    assert!(parser.screen().contents().contains("\nshell> "));
    assert_eq!(parser.screen().fgcolor(), vt100::Color::Default);
    assert!(!parser.screen().hide_cursor());
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_ctrl_c_keeps_partial_output_without_waiting_for_response() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(config_path(dir.path(), "settings.yml"), fixture_settings()).unwrap();
        std::fs::write(
            config_path(dir.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_existing_files(dir.path());
        pty.ready();
        pty.input("pending-task\r");
        let path = if protocol == "completion" {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        };
        let (request, _) = receive_at(&server, path);
        let mut stream = SseReply::new(request);
        streaming_delta(&mut stream, protocol, "partial-retained 中😀");
        pty.text("partial-retained");
        pty.input("\x03");
        pty.wait_for("退出后保留未完成的流式输出", |screen| {
            !screen.alternate_screen()
                && screen.contents().contains("partial-retained 中😀")
                && background_color(screen, "pending-task") == USER_BACKGROUND
                && background_color(screen, "partial-retained") == ASSISTANT_BACKGROUND
        });
        pty.finish();
        stream.finish();
        no_more(&server);
    }
}
