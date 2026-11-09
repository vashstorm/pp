// 等待全部正文进入原生历史或当前视口，不再把一次输入回显当作历史队列清空。
fn native_wait_complete(pty: &mut Pty, prefix: &str, count: usize) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let history = pty.native_history().0;
        if (0..count).all(|index| history.matches(&format!("{prefix}{index:03}")).count() == 1) { return; }
        assert!(Instant::now() < deadline, "原生历史未完整提交：{history}");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn native_tui_resume_hides_cursor_until_history_rebuild_finishes_both_protocols() {
    for protocol in ["completion", "response"] {
        let provider = server();
        let home = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            home.path(),
            Some("resume-cursor-task"),
            fixture_settings(),
            &response_models(&provider, Some(protocol)),
        );
        let (request, _) = receive_at(&provider, api_path(protocol));
        let answer = format!(
            "```text\n{}\n```",
            (0..180)
                .map(|i| format!("resume-cursor-{i:03} 中😀"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        diff_blocks_final(request, protocol, &answer);
        assert!(cli.finish().status.success());
        let id = saved_session_id(home.path());

        for startup in [true, false] {
            let args = if startup {
                vec!["-r", id.as_str()]
            } else {
                vec![]
            };
            let mut pty = Pty::start_with_args(home.path(), home.path(), &args);
            let before = if startup {
                0
            } else {
                pty.ready();
                pty.input("/resume\r");
                pty.wait_for("恢复面板已列出目标会话", |screen| {
                    let text = screen.contents();
                    text.contains("resume-cursor-task")
                        && text.lines().any(|line| line.starts_with('>'))
                });
                let before = pty.raw.len();
                pty.input("\r");
                before
            };
            native_wait_complete(&mut pty, "resume-cursor-", 180);
            pty.wait_for("历史重建后光标回到输入框", cancel_cursor_on_input_row);
            let output = String::from_utf8_lossy(&pty.raw[before..]);
            let first = output.find("resume-cursor-000").expect("历史首行");
            let last = output.rfind("resume-cursor-179").expect("历史尾行");
            let hide = output.find("\x1b[?25l").expect("重建前隐藏光标");
            let show = output.find("\x1b[?25h").expect("重建后恢复光标");
            assert!(
                hide < first && show > last,
                "历史重建期间提前显示光标：{output:?}"
            );
            assert_eq!(
                output.matches("\x1b[?25h").count(),
                1,
                "历史批次反复显示光标"
            );
            assert!(
                output.matches("\x1b[?2026h").count() > 2,
                "未覆盖多个历史批次"
            );
            pty.input("resume-cursor-draft");
            pty.draft("resume-cursor-draft");
            pty.input("\x03");
            pty.finish();
            no_more(&provider);
        }
    }
}

// 原生历史必须在运行中提交；退出只收尾，不能重新输出早期会话。

#[test]
fn native_tui_streaming_keeps_input_cursor_visible_both_protocols() {
    for protocol in ["completion", "response"] {
        let provider = server();
        let (_home, mut pty) = context_tui_start(&provider, protocol, 100_000, 80);
        pty.input("cursor-stream-task\r");
        let (request, _) = pty.request_at(&provider, api_path(protocol));
        let mut stream = SseReply::new(request);
        let mut answer = String::from("cursor-stream-start");
        streaming_delta(&mut stream, protocol, &answer);
        pty.text(&answer);
        let draft = "cursor-draft 中";
        pty.input(draft);
        pty.draft(draft);
        let before = pty.raw.len();
        for batch in 0..3 {
            // 每批等待可见标记，完成后继续核对原生历史提交。
            let delta = (0..40).map(|line| format!("\n\ncursor-stream-{batch}-{line:03}"))
                .collect::<String>();
            answer.push_str(&delta);
            streaming_delta(&mut stream, protocol, &delta);
            pty.text(&format!("cursor-stream-{batch}-039"));
            pty.wait_for("流式输出后草稿光标保持可见", |screen| {
                !screen.hide_cursor()
                    && screen.contents().lines().position(|line| line.contains(draft))
                        == Some(usize::from(screen.cursor_position().0))
            });
        }
        streaming_complete(&mut stream, protocol, &answer);
        stream.finish();
        pty.completed();
        pty.draft(draft);
        for batch in 0..3 {
            native_wait_complete(&mut pty, &format!("cursor-stream-{batch}-"), 40);
        }
        let output = String::from_utf8_lossy(&pty.raw[before..]);
        assert!(!output.contains("\x1b[?25l"), "流式输出重复隐藏光标：{output:?}");
        assert!(!output.contains("\x1b[?25h"), "流式输出重复显示光标：{output:?}");
        pty.input("\x03");
        pty.finish();
        no_more(&provider);
    }
}

#[test]
fn native_tui_long_history_exits_without_replay_both_protocols() {
    for protocol in ["completion", "response"] {
        let provider = server();
        let (_home, mut pty) = context_tui_start(&provider, protocol, 100_000, 80);
        pty.input("native-history-task\r");
        let (request, _) = pty.request_at(&provider, api_path(protocol));
        let answer = (0..140).map(|i| format!("    native-history-{i:03} 中😀"))
            .collect::<Vec<_>>().join("\n");
        diff_blocks_final(request, protocol, &answer);
        pty.text("native-history-139");
        pty.ready();
        // 草稿回显证明前一轮的全部历史批次已经处理，不能靠等待时间推测。
        pty.input("exit-barrier");
        pty.draft("exit-barrier");
        assert!(!pty.parser.screen().alternate_screen());
        native_wait_complete(&mut pty, "native-history-", 140);
        let (history, _) = pty.native_history();
        assert_eq!(history.matches("native-history-000").count(), 1, "{history}");
        assert_eq!(history.matches("native-history-100").count(), 1, "{history}");
        let before = pty.raw.len();
        pty.input("\x03");
        pty.wait_for("退出已恢复终端模式", |screen| !screen.bracketed_paste());
        let exit = String::from_utf8_lossy(&pty.raw[before..]);
        assert!(!exit.contains("native-history-"), "退出重放了正文：{exit:?}");
        assert!(!exit.contains("\x1b[2J") && !exit.contains("\x1b[3J"));
        let (history, _) = pty.native_history();
        for index in 0..140 {
            let marker = format!("native-history-{index:03}");
            assert_eq!(history.matches(&marker).count(), 1, "{marker}\n{history}");
        }
        pty.finish();
        no_more(&provider);
    }
}

#[test]
fn native_tui_new_clears_native_history_without_deleting_saved_session() {
    let provider = server();
    let (home, mut pty) = context_tui_start(&provider, "completion", 100_000, 80);
    pty.input("native-old-session\r");
    let (request, _) = pty.request(&provider);
    let answer = (0..80).map(|i| format!("    native-old-{i:03}"))
        .collect::<Vec<_>>().join("\n");
    final_reply(request, &answer);
    pty.text("native-old-079");
    // 草稿回显只证明输入可编辑，清理命令需要等待模型任务完成。
    pty.completed();
    pty.input("reset-barrier");
    pty.draft("reset-barrier");
    native_wait_complete(&mut pty, "native-old-", 80);
    assert!(pty.native_history().0.contains("native-old-000"));
    let id = saved_session_id(home.path());
    pty.input("\x01\x0b/new\r");
    pty.wait_for_raw(b"\x1b[3J");
    pty.text("Session & workflow");
    pty.ready();
    let history = pty.native_history().0;
    assert!(!history.contains("native-old-"), "{history}");
    assert!(resume_history_journal_dir(home.path()).join(format!("{id}.jsonl")).exists());
    pty.input("\x03");
    pty.finish();
    no_more(&provider);
}

#[test]
fn native_tui_long_partial_exit_preserves_uncommitted_rows_both_protocols() {
    for protocol in ["completion", "response"] {
        let provider = server();
        let (_home, mut pty) = context_tui_start(&provider, protocol, 100_000, 80);
        pty.input("native-partial-task\r");
        let (request, _) = pty.request_at(&provider, api_path(protocol));
        let mut stream = SseReply::new(request);
        let answer = (0..90).map(|i| format!("partial-native-{i:03}\n\n"))
            .collect::<String>();
        streaming_delta(&mut stream, protocol, &answer);
        pty.text("partial-native-089");
        pty.input("\x03");
        pty.wait_for("不等待网络即可退出", |screen| !screen.bracketed_paste());
        let history = pty.native_history().0;
        for index in 0..90 {
            let marker = format!("partial-native-{index:03}");
            assert_eq!(history.matches(&marker).count(), 1, "{marker}\n{history}");
        }
        pty.finish();
        stream.finish();
        no_more(&provider);
    }
}

#[test]
fn slow_native_tui_resize_between_batches_keeps_each_source_line_once() {
    let provider = server();
    let (_home, mut pty) = context_tui_start(&provider, "completion", 100_000, 80);
    pty.input("native resize task\r");
    let (request, _) = pty.request(&provider);
    let answer = format!("```txt\n{}\n```", (0..96).map(|index| format!("mid-resize-{index:03} {}", "abcdefghij".repeat(6))).collect::<Vec<_>>().join("\n"));
    final_reply(request, &answer);
    pty.wait_for_raw(b"mid-resize-000");
    pty.resize(30, 40);
    native_wait_complete(&mut pty, "mid-resize-", 96);
    pty.input("\x03");
    pty.finish();
    no_more(&provider);
}
