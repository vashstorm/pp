// `!command` 本地 shell 的 TUI 回归：真实二进制 + PTY + 回环 provider。覆盖输出
// 卡片/Ctrl + O 展开/输入历史与模型请求·会话隔离、忙碌/Plan/pending 三种拒绝、
// Esc 取消回收整个进程组、Ctrl + J 多行与 bash 超时。全部以可见屏幕、HTTP、
// PID/文件 marker 为栅栏，不用 sleep 推测 UI 阶段。辅助项均以 `bang_shell_`
// 前缀命名，不新增 use 声明，避免与兄弟 include 文件冲突。

fn bang_shell_start(
    server: &TestServer,
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
fn bash_task_polls_reuse_one_card_and_keep_bounded_tail_both_protocols() {
    let protocol = "response";
        let provider = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = ask_tui_start(&provider, home.path(), project.path(), protocol);
        pty.input("background output\r");
        let (request, _) = pty.request_at(&provider, api_path(protocol));
        let args = json!({"command":"for i in {0..19}; do printf 'task-line-%02d\\n' \"$i\"; done; touch task-output-ready", "mode":"background"});
        let make_call = |id: &str, name: &str, args: Value| {
            if protocol == "completion" {
                call(id, name, args)
            } else {
                response_call(id, name, args)
            }
        };
        ask_reply_calls(request, protocol, json!([make_call("start", "bash", args)]));
        let (mut request, body) = pty.request_at(&provider, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        let task_id = outputs.last().unwrap().1["task_id"].as_str().unwrap().to_owned();
        pty.wait_for("后台命令已写完输出", |_| project.path().join("task-output-ready").is_file());
        for poll in 0..5 {
            ask_reply_calls(request, protocol, json!([make_call(
                &format!("poll-{poll}"), "bash_task", json!({"task_id":task_id,"action":"status"})
            )]));
            let (next, body) = pty.request_at(&provider, api_path(protocol));
            request = next;
            let outputs = cancel_tool_outputs(&body, protocol);
            assert_eq!(outputs.last().unwrap().1["task_id"], task_id);
            pty.text(&format!("Task {task_id}:"));
            let history = pty.native_history().0;
            assert_eq!(history.lines().filter(|line| line.contains('╭') && line.contains("Bash")).count(), 1, "{history}");
            assert!(!history.contains("Bash task ·"), "{history}");
        }
        let args = json!({"task_id":task_id, "action":"stop"});
        ask_reply_calls(request, protocol, json!([make_call("stop", "bash_task", args)]));
        let (request, body) = pty.request_at(&provider, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        let stdout = outputs.last().unwrap().1["output"].as_str().unwrap();
        assert!(stdout.contains("task-line-00") && stdout.contains("task-line-19"));
        pty.wait_for("Bash task 快照仅显示最新十行", |screen| {
            let text = screen.contents();
            text.contains("task-line-19")
                && text.contains("10 earlier lines, showing 10 of 20")
                && !text.contains("task-line-09")
        });
        pty.input("保留快照草稿");
        pty.draft("保留快照草稿");
        pty.function_key('o');
        pty.text("task-line-00");
        pty.draft("保留快照草稿");
        ask_final_reply(request, protocol, "snapshot done");
        pty.text("snapshot done");
        pty.completed();
        // Ctrl + O 使用备用屏幕浏览；返回主屏幕后才能核对原生历史提交。
        pty.input("\x1b");
        pty.wait_for("退出工具展开浏览", |screen| !screen.alternate_screen());
        let deadline = Instant::now() + DEADLINE;
        let history = loop {
            let history = pty.native_history().0;
            if history.lines().any(|line| line.contains('╭') && line.contains("Bash")) {
                break history;
            }
            assert!(Instant::now() < deadline, "Bash 卡片未提交到原生历史：{history}");
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(history.lines().filter(|line| line.contains('╭') && line.contains("Bash")).count(), 1, "{history}");
        pty.input("\x03");
        pty.finish();
        no_more(&provider);
}

