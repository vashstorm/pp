// 运行中消息排队的真实二进制场景（计划验证项 3、4、5）：FIFO follow-up 与多行
// Unicode 草稿、取消/供应商错误后暂停并等待功能键 R、取消收尾忙碌期拒绝入队、
// Esc 与已提交回复的竞争不自动续跑、取回原始模板 slash 且只执行编辑后的新值、
// 非空草稿阻止取回且光标不变、Tab 不取回、Esc 只关菜单不取消任务、100×30 与
// 40×10 下队列列表/草稿/原生光标不被覆盖。全部用真实 PTY、回环 fixture 与屏幕/
// HTTP/文件 marker 作栅栏，不用 sleep 猜测阶段。辅助函数一律使用
// `runtime_input_` 前缀，不新增 use 声明，避免与兄弟 include 文件冲突。

fn runtime_input_empty_editor(pty: &mut Pty) {
    pty.wait_for("empty editor draft after enqueue", |screen| {
        let contents = screen.contents();
        let lines: Vec<&str> = contents.lines().collect();
        lines
            .windows(2)
            .any(|pair| pair[0].trim_end() == "❯" && pair[1].starts_with('─'))
    });
}

fn runtime_input_menu_visible(pty: &mut Pty, labels: &[&str]) {
    let expected: Vec<String> = labels.iter().map(|label| label.to_string()).collect();
    pty.wait_for("pending input menu visible", |screen| {
        let text = screen.contents();
        expected.iter().all(|label| text.contains(label.as_str()))
    });
}

fn runtime_input_menu_closed(pty: &mut Pty) {
    pty.wait_for("pending input menu closed", |screen| {
        let contents = screen.contents();
        !contents.contains("Steer #") && !contents.contains("Queued #")
    });
}

fn runtime_input_cancel_terminal(pty: &mut Pty, steering: usize, queued: usize) {
    pty.wait_for("cancel terminal state with paused queue", |screen| {
        input_is_ready(screen)
            && cancel_cursor_on_input_row(screen)
            && cancel_count_normalized(screen, CANCEL_DONE) == 1
    });
    runtime_input_counts(pty, steering, queued, true);
}

// 队列清空后计数行整行消失（0 条时不渲染），以此作为“全部应用”的屏幕栅栏。
fn runtime_input_queue_drained(pty: &mut Pty) {
    pty.wait_for("pending queue fully applied", |screen| {
        let text = screen.contents();
        !text.contains("Steering:") && !text.contains("Queued:")
    });
}

fn runtime_input_draft_visible(pty: &mut Pty, markers: &[&str]) {
    let expected: Vec<String> = markers.iter().map(|marker| marker.to_string()).collect();
    pty.wait_for("uncommitted draft preserved", |screen| {
        let text = screen.contents();
        expected.iter().all(|marker| text.contains(marker.as_str()))
    });
}

// 原生光标停在包含 marker 的那一行，返回该列；后续用同一列断言光标未被移动。
fn runtime_input_cursor_on(pty: &mut Pty, marker: &str) -> u16 {
    pty.wait_for("cursor on the draft tail line", |screen| {
        screen
            .contents()
            .lines()
            .position(|line| line.contains(marker))
            .is_some_and(|row| row == usize::from(screen.cursor_position().0))
    });
    pty.parser.screen().cursor_position().1
}

fn runtime_input_cursor_kept(pty: &mut Pty, marker: &str, column: u16) {
    pty.wait_for("draft cursor preserved", |screen| {
        screen.cursor_position().1 == column
            && screen
                .contents()
                .lines()
                .position(|line| line.contains(marker))
                .is_some_and(|row| row == usize::from(screen.cursor_position().0))
    });
}

// 场景 5 需要 /命令 模板在 pp 启动前就位；配置写法与 runtime_input_start 相同。
fn runtime_input_command_start(
    server: &TestServer,
    protocol: &str,
    command_body: &str,
) -> (tempfile::TempDir, Pty) {
    let home = runtime_input_home(server, protocol, "");
    install_command(home.path(), "qcmd.md", command_body);
    let mut pty = Pty::start_with_args(home.path(), home.path(), &[]);
    pty.ready();
    (home, pty)
}

// 场景 3：FIFO follow-up + 未提交多行 Unicode 草稿。Q2 只能在 Q1 成功后出现，
// 每条用户消息各一次，独立任务各自发起请求；草稿与原生光标全程保持。
#[test]
fn tui_runtime_input_followup_fifo_and_draft_both_protocols() {
    let protocol = "response";
        let server = server();
        let (_home, mut pty) = runtime_input_start(&server, protocol, "");
        let stream = runtime_input_live(&mut pty, &server, protocol, "fifo-task-U");

        pty.input("Q1\x1b\r");
        runtime_input_counts(&mut pty, 0, 1, false);
        runtime_input_empty_editor(&mut pty);
        pty.input("Q2\x1b\r");
        runtime_input_counts(&mut pty, 0, 2, false);
        runtime_input_empty_editor(&mut pty);

        pty.input("草稿 α✓\x0a第二行");
        runtime_input_draft_visible(&mut pty, &["草稿 α✓", "第二行"]);
        let column = runtime_input_cursor_on(&mut pty, "第二行");

        runtime_input_end_live(stream, protocol);

        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["fifo-task-U", "Q1"],
            "{protocol}: first follow-up must be Q1 exactly once"
        );
        runtime_input_counts(&mut pty, 0, 1, false);
        let mut q1 = request.start_sse().unwrap();
        streaming_delta(&mut q1, protocol, "fifo-q1-live");
        pty.text("fifo-q1-live");
        no_more(&server);

        streaming_complete(&mut q1, protocol, "fifo-q1-live");
        q1.finish();

        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["fifo-task-U", "Q1", "Q2"],
            "{protocol}: Q2 must wait for Q1 success and appear once"
        );
        assert_eq!(
            cancel_assistant_texts(&body, protocol),
            ["runtime-live-marker", "fifo-q1-live"],
            "{protocol}: each finished task contributes one assistant reply"
        );
        runtime_input_final(request, protocol, "fifo-q2-final");
        pty.text("fifo-q2-final");
        runtime_input_queue_drained(&mut pty);

        runtime_input_draft_visible(&mut pty, &["草稿 α✓", "第二行"]);
        runtime_input_cursor_kept(&mut pty, "第二行", column);

        pty.input("\x03");
        pty.finish();
        no_more(&server);
}

// 场景 8b：被 UserPromptSubmit 拒绝的补充输入显示拒绝原因与原文（不是成功的 You
// 卡片）；全部被拒绝时没有新增输入，不得再发一次模型请求。
#[test]
fn slow_hooks_tui_rejected_steering_card_needs_no_extra_request() {
    let protocol = "response";
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let reject = home.path().join("reject.sh");
        std::fs::write(
            &reject,
            "#!/bin/bash\npayload=$(cat)\nif [[ $payload == *\"reject-me\"* ]]; then\n  printf 'policy says no' >&2\n  exit 2\nfi\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&reject).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&reject, permissions).unwrap();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            "default_model: fixture/primary\nnotification_timeout_seconds: 2\n",
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "hooks.yml"),
            format!(
                "UserPromptSubmit:\n  - hooks:\n      - type: command\n        command: '{reject}'\n",
                reject = reject.display()
            ),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_args(home.path(), home.path(), &[]);
        pty.ready();

        let stream = runtime_input_live(&mut pty, &server, protocol, "hook-task-U");
        // 完成边界前入队：必须在提交边界被拒绝，绝不进入原生历史。
        pty.input("reject-me\r");
        runtime_input_counts(&mut pty, 1, 0, false);
        cancel_assert_no_request(&server);
        runtime_input_end_live(stream, protocol);
        pty.wait_for("rejected steering card", |screen| {
            let text = screen.contents();
            // 终端解析器不会保留原始 escape：出现 ESC 说明脚本正文漏进了屏幕。
            assert!(
                !text.contains('\u{1b}'),
                "{protocol}: hook 诊断不得注入原始终端转义：{text:?}"
            );
            text.contains("Rejected")
                && text.contains("Input rejected: policy says no")
                && text.contains("reject-me")
        });
        // 全部补充输入被拒绝：没有新增用户输入，不得再请求模型。
        pty.wait_for("task finished after rejected steering", |screen| {
            screen.contents().contains("Completed ·") && input_is_ready(screen)
        });
        cancel_assert_no_request(&server);

        pty.input("\x03");
        pty.finish();
        no_more(&server);
}
