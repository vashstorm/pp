// `!command` 本地 shell 的 TUI 回归：真实二进制 + PTY + 回环 provider。覆盖输出
// 卡片/Ctrl + O 展开/输入历史与模型请求·会话隔离、忙碌/Plan/pending 三种拒绝、
// Esc 取消回收整个进程组、Ctrl + J 多行与 bash 超时。全部以可见屏幕、HTTP、
// PID/文件 marker 为栅栏，不用 sleep 推测 UI 阶段。辅助项均以 `bang_shell_`
// 前缀命名，不新增 use 声明，避免与兄弟 include 文件冲突。

fn bang_shell_start(
    server: &Server,
    home: &Path,
    project: &Path,
    protocol: &str,
    settings_extra: &str,
) -> Pty {
    std::fs::write(
        config_path(home, "settings.yml"),
        format!(
            r#"{{"default_model":"fixture/primary","notification_timeout_seconds":2{settings_extra}}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        config_path(home, "models.yml"),
        response_models(server, Some(protocol)),
    )
    .unwrap();
    let mut pty = Pty::start_with_args(project, home, &[]);
    pty.ready();
    pty
}

fn bang_shell_sessions_dir(home: &Path) -> std::path::PathBuf {
    home.join(".pp/agent/sessions")
}

// bash 卡片结束栅栏：footer 行必须同时含 Wall 与 Timeout 或失败状态，
// 不能只用空闲输入框判断本地 shell 已结束。
fn bang_shell_wait_footer(pty: &mut Pty, description: &str, status: &str) {
    let compact = status.replace(' ', "");
    pty.wait_for(description, |screen| {
        screen
            .contents()
            .lines()
            .rfind(|line| {
                line.contains("╰") && (line.contains("Wall:") || line.contains("Elapsed:"))
            })
            .is_some_and(|line| {
                let line = line.split_whitespace().collect::<String>();
                line.contains("Wall:")
                    && line.contains(&compact)
                    && (status != "Timeout:"
                        || (!line.contains("Exit:")
                            && !line.contains("cancelled")
                            && !line.contains("timedout")))
            })
    });
}

// 本地 shell 受控结束：状态提示消失且输入区恢复。
fn bang_shell_wait_done(pty: &mut Pty) {
    pty.wait_for("本地 shell 结束且输入区恢复", |screen| {
        let text = screen.contents();
        input_is_ready(screen)
            && !text.contains("Running shell...")
            && !text.contains("Cancelling shell...")
    });
}

#[test]
fn bash_running_output_scrolls_inside_fixed_window_both_protocols() {
    for protocol in ["completion", "response"] {
        let provider = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        // 文件门闩由屏幕断言后显式放行，轮询间隔不用于推测输出或绘制阶段。
        std::fs::write(project.path().join("scroll-output.sh"), r#"
for i in {0..2}; do printf 'scroll-line-%02d\n' "$i"; done
while [[ ! -e scroll-next ]]; do sleep 0.01; done
for i in {3..15}; do printf 'scroll-line-%02d\n' "$i"; done
while [[ ! -e scroll-finish ]]; do sleep 0.01; done
"#).unwrap();
        let mut pty = ask_tui_start(&provider, home.path(), project.path(), protocol);
        pty.input("fixed bash output\r");
        let (request, _) = pty.request_at(&provider, api_path(protocol));
        pty.input("保留输入草稿");
        pty.draft("保留输入草稿");
        let args = json!({"command":"bash scroll-output.sh"});
        let tool = if protocol == "completion" {
            call("scroll", "bash", args)
        } else {
            response_call("scroll", "bash", args)
        };
        ask_reply_calls(request, protocol, json!([tool]));
        pty.wait_for("初始输出与运行边框已绘制", |screen| {
            let text = screen.contents();
            text.contains("scroll-line-02") && text.contains("Elapsed:")
        });
        let row_of = |screen: &vt100::Screen, needle: &str| {
            screen.contents().lines().position(|line| line.contains(needle)).unwrap()
        };
        let anchors = ["$ bash scroll-output.sh", "Output", "scroll-line-02", "Elapsed:"]
            .map(|needle| row_of(pty.parser.screen(), needle));
        assert!(!pty.parser.screen().alternate_screen());
        assert_eq!(anchors[3] - anchors[1], 12, "10 行正文及一行提示");

        std::fs::write(project.path().join("scroll-next"), "").unwrap();
        pty.wait_for("固定窗口自动移出旧行并显示最新行", |screen| {
            let text = screen.contents();
            text.contains("scroll-line-15") && !text.contains("scroll-line-02")
        });
        let screen = pty.parser.screen();
        let text = screen.contents();
        assert!(text.contains("scroll-line-06") && !text.contains("scroll-line-05"), "{text}");
        assert!(text.contains("6 earlier lines, showing 10 of 16"), "{text}");
        for (needle, row) in ["$ bash scroll-output.sh", "Output", "scroll-line-15", "Elapsed:"]
            .into_iter().zip(anchors)
        {
            assert_eq!(row_of(screen, needle), row, "{protocol}: {needle}\n{text}");
        }
        assert!(!screen.alternate_screen());
        pty.draft("保留输入草稿");

        std::fs::write(project.path().join("scroll-finish"), "").unwrap();
        let (request, body) = pty.request_at(&provider, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(outputs.len(), 1);
        let stdout = outputs[0].1["stdout"].as_str().unwrap();
        assert!(stdout.contains("scroll-line-00") && stdout.contains("scroll-line-15"));
        ask_final_reply(request, protocol, "fixed output done");
        pty.text("fixed output done");
        pty.ready();
        pty.input("\x0f");
        pty.wait_for("展开仍能查看滚出窗口的原始输出", |screen| {
            screen.contents().contains("scroll-line-00")
        });
        pty.draft("保留输入草稿");
        pty.input("\x03");
        pty.finish();
        no_more(&provider);
    }
}

#[test]
fn bang_shell_tui_output_and_history() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = bang_shell_start(&server, home.path(), project.path(), protocol, "");
        let sessions = bang_shell_sessions_dir(home.path());
        assert!(!sessions.exists(), "{protocol}: 尚无任务却已有会话目录");

        // 首个 ! 只切换模式，输入框与原生光标仍停在空正文位置。
        pty.input("!");
        pty.wait_for("shell 模式隐藏切换前缀", |screen| {
            let (row, column) = screen.cursor_position();
            screen.contents().lines().any(|line| line.contains('❯'))
                && screen.cell(row, column.saturating_sub(1)).is_some_and(|cell| {
                    cell.contents().trim().is_empty()
                })
                && !screen.contents().lines().any(|line| line.contains("❯ !"))
                && screen.cell(row.saturating_sub(1), column).is_some_and(|cell| {
                    cell.fgcolor() == vt100::Color::Rgb(255, 121, 198)
                })
        });
        pty.input("\x7f");

        // stdout/stderr 双流 + 14 行输出 + exit 7：以卡片 footer 判定结束。
        pty.input("!printf 'tui-out-marker\\n'; printf 'tui-err-marker\\n' >&2; for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14; do printf 'fold-%02d\\n' \"$i\"; done; exit 7\r");
        bang_shell_wait_footer(&mut pty, "exit 7 卡片 footer", "Exit: 7");
        pty.wait_for("折叠卡片显示尾部行与 stderr 段", |screen| {
            let text = screen.contents();
            text.contains("(Ctrl + O to expand)")
                && text.contains("[stderr]")
                && text.contains("tui-err-marker")
                && text.contains("fold-14")
                && !text.contains("fold-06")
        });
        // Ctrl + O 展开完整输出。
        pty.input("\x0f");
        pty.wait_for("展开后可见最早输出行", |screen| {
            let text = screen.contents();
            text.contains("fold-01") && text.contains("fold-06")
        });

        // 第二条命令成功执行。
        pty.input("!printf 'tui-second\\n'\r");
        bang_shell_wait_footer(&mut pty, "第二条命令 exit 0", "Timeout:");
        pty.text("tui-second");

        // ↑ 回填 shell 命令并恢复模式，切换用的 ! 不显示。
        pty.input("\x1b[A");
        pty.draft("printf 'tui-second\\n'");
        pty.input("\x01\x0b");

        // shell-only：无模型请求、无会话文件、scratch 收尾干净。
        cancel_assert_no_request(&server);
        assert!(!sessions.exists(), "{protocol}: 本地 shell 不得创建会话");
        cancel_assert_no_scratch(project.path());

        // 普通模型任务：请求与 journal 均不含本地 shell 命令/输出或工具交换。
        pty.input("tui-shell-isolated-prompt\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        let serialized = serde_json::to_string(&body).unwrap();
        for marker in [
            "tui-out-marker",
            "tui-err-marker",
            "tui-second",
            "fold-01",
            "fold-14",
            "local-shell",
            "\"tool_calls\"",
            "function_call_output",
        ] {
            assert!(
                !serialized.contains(marker),
                "{protocol}: 本地 shell 内容泄漏进模型请求: {serialized}"
            );
        }
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["tui-shell-isolated-prompt"],
            "{protocol}"
        );
        runtime_input_final(request, protocol, "tui-shell-isolated-reply");
        pty.text("tui-shell-isolated-reply");
        pty.ready();

        let files: Vec<_> = std::fs::read_dir(&sessions)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(files.len(), 1, "{protocol}: 恰好一个会话文件");
        let journal = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(
            journal.contains("tui-shell-isolated-prompt"),
            "{protocol}: {journal}"
        );
        for marker in [
            "tui-out-marker",
            "tui-err-marker",
            "tui-second",
            "fold-01",
            "local-shell",
        ] {
            assert!(
                !journal.contains(marker),
                "{protocol}: 本地 shell 内容泄漏进会话: {journal}"
            );
        }
        no_more(&server);
        pty.input("\x03");
        pty.finish();
    }
}

#[test]
fn bang_shell_tui_busy_and_plan_reject() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = bang_shell_start(&server, home.path(), project.path(), protocol, "");
        let forbidden = project.path().join("forbidden");

        // 忙碌：模型请求已到达但被挂住，提交 !command 只拒绝并保留草稿。
        pty.input("busy-shell-task\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["busy-shell-task"],
            "{protocol}"
        );
        pty.input("!touch forbidden\r");
        pty.text("Finish or cancel the current task");
        pty.draft("touch forbidden");
        assert!(!forbidden.exists(), "{protocol}");
        runtime_input_final(request, protocol, "busy-shell-released");
        pty.text("busy-shell-released");
        pty.ready();
        assert!(!forbidden.exists(), "{protocol}");
        pty.wait_for("忙碌拒绝不产生待处理消息", |screen| {
            !screen.contents().contains("Steering:")
        });
        cancel_assert_no_request(&server);
        pty.input("\x01\x0b");

        // Plan 模式拒绝：本地提示、不执行、无模型请求。
        pty.input("/plan\r");
        plan_wait_mode(&mut pty, "Plan");
        pty.input("!touch forbidden\r");
        pty.text("Shell commands are unavailable in Plan mode.");
        pty.draft("touch forbidden");
        assert!(!forbidden.exists(), "{protocol}");
        pty.input("\x01\x0b");
        pty.input("/plan\r");
        plan_wait_execute(&mut pty);
        cancel_assert_no_request(&server);

        // Ready 但队列暂停且有 pending：同样拒绝并保留原队列与草稿。
        let mut stream = runtime_input_live(&mut pty, &server, protocol, "queue-shell-task");
        pty.input("QS\r");
        runtime_input_counts(&mut pty, 1, 0, false);
        if protocol == "completion" {
            stream.event(json!({
                "error": {"message": "queue-shell-boom", "type": "server_error"}
            }));
        } else {
            stream.event(json!({
                "type": "error", "sequence_number": 9, "code": "server_error",
                "message": "queue-shell-boom", "param": null
            }));
        }
        stream.finish();
        runtime_input_failed_terminal(&mut pty, 1, 0);
        pty.input("!touch forbidden\r");
        pty.text("Resolve pending messages");
        pty.draft("touch forbidden");
        runtime_input_counts(&mut pty, 1, 0, true);
        assert!(!forbidden.exists(), "{protocol}");
        cancel_assert_no_request(&server);
        pty.input("\x03");
        pty.finish();
        assert!(!forbidden.exists(), "{protocol}");
    }
}

#[test]
fn bang_shell_tui_escape_reaps_process_group() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = bang_shell_start(&server, home.path(), project.path(), "completion", "");
    let sessions = bang_shell_sessions_dir(home.path());

    // 父 shell 与子 subshell 都忽略 TERM 并持续运行：Esc 必须用 grace 后的
    // KILL 回收整个进程组。
    pty.input("!trap '' TERM; (trap '' TERM; sleep 30) & child=$!; printf '%s' \"$$\" > shell-esc-parent.pid; printf '%s' \"$child\" > shell-esc-child.pid; printf 'shell-esc-ready\\n'; wait \"$child\"\r");
    cancel_wait_pid_files(
        project.path(),
        &["shell-esc-parent.pid", "shell-esc-child.pid"],
    );
    pty.text("shell-esc-ready");

    // shell 运行中普通提交既不发起模型请求，也不进入待处理队列，草稿保留。
    pty.input("esc-shell-draft\r");
    pty.draft("esc-shell-draft");
    pty.wait_for("运行 shell 时无队列项", |screen| {
        !screen.contents().contains("Steering:")
    });
    cancel_assert_no_request(&server);

    // Esc 取消；重复 Esc 幂等。
    pty.input("\x1b");
    pty.text("Cancelling shell...");
    pty.input("\x1b");
    cancel_wait_pid_dead(&project.path().join("shell-esc-parent.pid"));
    cancel_wait_pid_dead(&project.path().join("shell-esc-child.pid"));
    cancel_assert_no_scratch(project.path());
    bang_shell_wait_done(&mut pty);
    pty.text("Command cancelled; side effects were not rolled back.");
    bang_shell_wait_footer(&mut pty, "取消卡片 footer", "cancelled");
    // 取消不吞掉运行中编辑的草稿。
    pty.draft("esc-shell-draft");
    assert!(!sessions.exists(), "本地 shell 不创建会话");

    // 清空草稿后仍可执行下一条 shell。
    pty.input("\x01\x0b");
    pty.input("!printf 'shell-esc-recovered\\n'\r");
    pty.text("shell-esc-recovered");
    bang_shell_wait_footer(&mut pty, "取消后恢复执行 exit 0", "Timeout:");
    cancel_assert_no_scratch(project.path());
    no_more(&server);
    assert!(!sessions.exists());
    pty.input("\x03");
    pty.finish();
}

#[test]
fn bang_shell_tui_multiline_and_timeout() {
    // Ctrl + J 构造多行 !command：一次交给 bash，两条 printf 都真实输出。
    {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = bang_shell_start(&server, home.path(), project.path(), "completion", "");
        pty.input("!printf 'multi-one\\n'");
        pty.input("\x0a");
        pty.input("printf 'multi-two\\n'");
        pty.wait_for("多行草稿保留两行", |screen| {
            let text = screen.contents();
            text.contains("printf 'multi-one") && text.contains("printf 'multi-two")
        });
        pty.input("\r");
        pty.text("multi-one");
        pty.text("multi-two");
        bang_shell_wait_footer(&mut pty, "多行命令 exit 0", "Timeout:");
        cancel_assert_no_scratch(project.path());
        cancel_assert_no_request(&server);
        no_more(&server);
        pty.input("\x03");
        pty.finish();
    }
    // bash_timeout_seconds: 1：超时以真实 Wall/failed 终态为栅栏，之后仍可执行。
    {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = bang_shell_start(
            &server,
            home.path(),
            project.path(),
            "completion",
            ",\"bash_timeout_seconds\":1",
        );
        pty.input("!printf 'timeout-live\\n'; sleep 30\r");
        pty.text("timeout-live");
        bang_shell_wait_footer(&mut pty, "一秒超时的卡片终态", "timed out");
        bang_shell_wait_done(&mut pty);
        pty.input("!printf 'timeout-recovered\\n'\r");
        pty.text("timeout-recovered");
        bang_shell_wait_footer(&mut pty, "超时后恢复执行 exit 0", "Timeout:");
        cancel_assert_no_scratch(project.path());
        no_more(&server);
        pty.input("\x03");
        pty.finish();
    }
}
