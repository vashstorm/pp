// 等待全部正文进入原生历史或当前视口，不再把一次输入回显当作历史队列清空。
// resize 时会同时发生解析器重排与应用重绘，跨 40 列换行边界的行在重建的
// 原生历史里可能出现两次；这里只要求每个源行都不丢，不用“恰好一次”当栅栏。
fn native_wait_complete(pty: &mut Pty, prefix: &str, count: usize) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let history = pty.native_history().0;
        if (0..count).all(|index| history.contains(&format!("{prefix}{index:03}"))) { return; }
        assert!(Instant::now() < deadline, "原生历史未完整提交：{history}");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn native_tui_resume_hides_cursor_until_history_rebuild_finishes_both_protocols() {
    let protocol = "response";
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

// 原生历史必须在运行中提交；退出只收尾，不能重新输出早期会话。

