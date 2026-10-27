// 运行中消息排队的真实二进制场景（计划验证项 3、4、5）：FIFO follow-up 与多行
// Unicode 草稿、取消/供应商错误后暂停并等待功能键 R、取消收尾忙碌期拒绝入队、
// Esc 与已提交回复的竞争不自动续跑、取回原始模板 slash 且只执行编辑后的新值、
// 非空草稿阻止取回且光标不变、Tab 不取回、Esc 只关菜单不取消任务、100×30 与
// 40×10 下队列列表/草稿/原生光标不被覆盖。全部用真实 PTY、tiny_http 与屏幕/
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

fn runtime_input_cancel_terminal(pty: &mut Pty, notes: usize, steering: usize, queued: usize) {
    pty.wait_for("cancel terminal state with paused queue", |screen| {
        input_is_ready(screen)
            && cancel_cursor_on_input_row(screen)
            && cancel_count_normalized(screen, CANCEL_NOTE) == notes
    });
    runtime_input_counts(pty, steering, queued, true);
}

fn runtime_input_failed_terminal(pty: &mut Pty, steering: usize, queued: usize) {
    pty.wait_for("provider failure terminal state", |screen| {
        input_is_ready(screen) && screen.contents().contains("✻ Failed")
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
    server: &Server,
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
    for protocol in ["completion", "response"] {
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
        let mut q1 = SseReply::new(request);
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
}

// 场景 4a：真实 Esc 取消（先 SSE 打开、后 bash gate 工具批次）两次暂停队列；
// 取消收尾忙碌期 Enter/AltEnter 不入队；功能键 R 先 Steering 后 FollowUp，
// 草稿保持，取消本身不触发任何 HTTP。
#[test]
fn tui_runtime_input_cancel_pauses_queue_and_rejects_busy_cleanup() {
    for protocol in ["completion", "response"] {
        let server = server();
        let (home, mut pty) = runtime_input_start(&server, protocol, "");
        let project = home.path();
        let stream = runtime_input_live(&mut pty, &server, protocol, "cancel-task-U");

        pty.input("S1\r");
        runtime_input_counts(&mut pty, 1, 0, false);
        pty.input("Q1\x1b\r");
        runtime_input_counts(&mut pty, 1, 1, false);
        pty.input("草稿d");
        pty.draft("草稿d");

        pty.input("\x1b");
        runtime_input_cancel_terminal(&mut pty, 1, 1, 1);
        drop(stream);
        pty.draft("草稿d");
        no_more(&server);

        pty.function_key('r');
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["cancel-task-U", "S1"],
            "{protocol}: resumed queue must start with the steering message"
        );
        runtime_input_counts(&mut pty, 0, 1, false);
        runtime_input_calls(
            request,
            protocol,
            &[("hold", "bash", json!({"command": CANCEL_BATCH_COMMAND}))],
        );
        cancel_wait_pid_files(project, &["bash.pid", "bash-child.pid"]);

        // bash 仍活着（忽略 TERM，1 秒 grace 后才 KILL）时一定处于 Cancelling：
        // 此窗口内的 Enter/AltEnter 不入队，草稿只被普通编辑追加。
        pty.input("\x1b");
        pty.text("Cancelling");
        pty.input("X\r");
        pty.input("Y\x1b\r");
        cancel_wait_pid_dead(&project.join("bash.pid"));
        cancel_wait_pid_dead(&project.join("bash-child.pid"));
        cancel_assert_no_scratch(project);
        runtime_input_cancel_terminal(&mut pty, 2, 0, 1);
        pty.draft("草稿dXY");
        no_more(&server);

        pty.function_key('r');
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["cancel-task-U", "S1", "Q1"],
            "{protocol}: follow-up resumes after the steering task"
        );
        assert_eq!(cancel_called_ids(&body, protocol), ["hold"]);
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(outputs.len(), 1, "{protocol}: {outputs:?}");
        assert_eq!(outputs[0].0, "hold");
        assert_eq!(outputs[0].1["cancelled"], true);
        runtime_input_final(request, protocol, "cancel-resumed-final");
        pty.text("cancel-resumed-final");
        runtime_input_queue_drained(&mut pty);
        pty.draft("草稿dXY");

        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}

// 场景 4b：provider 错误终态（既有 fixture 格式的 SSE error 事件）暂停队列；
// 功能键 R 前无任何请求，恢复后先 Steering 再 FollowUp，草稿保持。
#[test]
fn tui_runtime_input_provider_error_pauses_queue_until_resume() {
    for protocol in ["completion", "response"] {
        let server = server();
        let (_home, mut pty) = runtime_input_start(&server, protocol, "");
        let mut stream = runtime_input_live(&mut pty, &server, protocol, "error-task-U");

        pty.input("ES\r");
        runtime_input_counts(&mut pty, 1, 0, false);
        pty.input("EQ\x1b\r");
        runtime_input_counts(&mut pty, 1, 1, false);
        pty.input("errdraft");
        pty.draft("errdraft");

        if protocol == "completion" {
            stream.event(json!({
                "error":{"message":"runtime-input-provider-boom","type":"server_error"}
            }));
        } else {
            stream.event(json!({
                "type":"error","sequence_number":9,"code":"server_error",
                "message":"runtime-input-provider-boom","param":null
            }));
        }
        stream.finish();
        runtime_input_failed_terminal(&mut pty, 1, 1);
        pty.draft("errdraft");
        no_more(&server);

        pty.function_key('r');
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["error-task-U", "ES"],
            "{protocol}: failed turn must not commit; steering resumes first"
        );
        runtime_input_final(request, protocol, "error-recovered-steer");
        pty.text("error-recovered-steer");

        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["error-task-U", "ES", "EQ"],
            "{protocol}: follow-up runs as its own task after steering"
        );
        assert_eq!(
            cancel_assistant_texts(&body, protocol),
            ["error-recovered-steer"],
            "{protocol}: the steering task committed its own reply"
        );
        runtime_input_final(request, protocol, "error-recovered-final");
        pty.text("error-recovered-final");
        runtime_input_queue_drained(&mut pty);
        pty.draft("errdraft");

        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}

// 场景 5：功能键 Q 列表、Tab 不取回、Esc 只关菜单不取消任务、非空草稿阻止取回
// 且光标与队列不变；取回模板命令回显原始 slash，编辑后重提交只执行新展开值，
// 旧展开值从不进入任何请求。
#[test]
fn tui_runtime_input_recall_template_and_resubmit_new_value() {
    for protocol in ["completion", "response"] {
        let server = server();
        let (_home, mut pty) =
            runtime_input_command_start(&server, protocol, "queued template body $ARGUMENTS");
        let stream = runtime_input_live(&mut pty, &server, protocol, "recall-task-U");

        pty.input("queued-old-value\x1b\r");
        runtime_input_counts(&mut pty, 0, 1, false);
        pty.input("/qcmd original-arg\x1b\r");
        runtime_input_counts(&mut pty, 0, 2, false);

        pty.function_key('q');
        runtime_input_menu_visible(
            &mut pty,
            &[
                "Queued #1",
                "Queued #2",
                "queued-old-value",
                "/qcmd original-arg",
                if cfg!(target_os = "macos") {
                    "Enter recall · ⌥ + R resume queue · Esc close"
                } else {
                    "Enter recall · Ctrl + R resume queue · Esc close"
                },
            ],
        );

        pty.input("\t");
        runtime_input_menu_visible(
            &mut pty,
            &[
                "Queued #1",
                if cfg!(target_os = "macos") {
                    "Enter recall · ⌥ + R resume queue · Esc close"
                } else {
                    "Enter recall · Ctrl + R resume queue · Esc close"
                },
            ],
        );
        runtime_input_empty_editor(&mut pty);
        no_more(&server);

        pty.input("\x1b");
        runtime_input_menu_closed(&mut pty);
        pty.text("runtime-live-marker");
        runtime_input_counts(&mut pty, 0, 2, false);
        no_more(&server);

        pty.input("draft-block");
        pty.draft("draft-block");
        pty.function_key('q');
        runtime_input_menu_visible(&mut pty, &["Queued #1", "Queued #2"]);
        let cursor = pty.parser.screen().cursor_position();
        pty.input("\r");
        pty.wait_for("recall blocked by existing draft", |screen| {
            screen
                .contents()
                .contains("Clear the draft before recalling a pending message.")
        });
        assert_eq!(
            pty.parser.screen().cursor_position(),
            cursor,
            "{protocol}: blocked recall must not move the cursor"
        );
        runtime_input_counts(&mut pty, 0, 2, false);
        pty.draft("draft-block");

        pty.input("\x1b");
        runtime_input_menu_closed(&mut pty);
        pty.input("\x01\x0b");
        runtime_input_empty_editor(&mut pty);

        pty.function_key('q');
        runtime_input_menu_visible(&mut pty, &["Queued #1", "Queued #2"]);
        pty.input("\x1b[B");
        pty.input("\r");
        pty.draft("/qcmd original-arg");
        runtime_input_menu_closed(&mut pty);
        runtime_input_counts(&mut pty, 0, 1, false);

        pty.input("\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f\x7f");
        pty.wait_for("slash argument deleted", |screen| {
            let text = screen.contents();
            text.contains("❯ /qcmd") && !text.contains("original-arg")
        });
        pty.input("edited-arg");
        pty.draft("/qcmd edited-arg");
        pty.input("\x1b\r");
        runtime_input_counts(&mut pty, 0, 2, false);
        runtime_input_empty_editor(&mut pty);

        runtime_input_end_live(stream, protocol);

        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["recall-task-U", "queued-old-value"],
            "{protocol}: FIFO dispatches the untouched plain message first"
        );
        runtime_input_final(request, protocol, "recall-first-final");
        pty.text("recall-first-final");

        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            [
                "recall-task-U",
                "queued-old-value",
                "queued template body edited-arg"
            ],
            "{protocol}: only the edited slash value may run; the recalled \
             template is re-expanded and the old expansion never sent"
        );
        runtime_input_final(request, protocol, "recall-second-final");
        pty.text("recall-second-final");
        runtime_input_queue_drained(&mut pty);

        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}

// 场景 5 补充：100×30 与 40×10 下功能键 Q 队列列表可见、菜单 Esc 只关列表不取消
// 任务、resize 后草稿与原生光标列不被覆盖；队列按 Steering → FollowUp 消化。
#[test]
fn tui_runtime_input_pending_menu_tab_esc_resize_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = server();
        let (_home, mut pty) = runtime_input_start(&server, protocol, "");
        let stream = runtime_input_live(&mut pty, &server, protocol, "resize-task-U");

        pty.input("steer-item\r");
        runtime_input_counts(&mut pty, 1, 0, false);
        pty.input("queued-item\x1b\r");
        runtime_input_counts(&mut pty, 1, 1, false);
        pty.input("resize草稿");
        pty.draft("resize草稿");
        let column = runtime_input_cursor_on(&mut pty, "resize草稿");

        pty.function_key('q');
        runtime_input_menu_visible(
            &mut pty,
            &[
                "Steer #1",
                "Queued #2",
                "steer-item",
                "queued-item",
                if cfg!(target_os = "macos") {
                    "Enter recall · ⌥ + R resume queue · Esc close"
                } else {
                    "Enter recall · Ctrl + R resume queue · Esc close"
                },
            ],
        );

        pty.resize(10, 40);
        runtime_input_menu_visible(&mut pty, &["Steer #1", "Queued #2"]);
        pty.input("\x1b");
        runtime_input_menu_closed(&mut pty);
        runtime_input_draft_visible(&mut pty, &["resize草稿"]);
        runtime_input_cursor_kept(&mut pty, "resize草稿", column);
        no_more(&server);

        pty.function_key('q');
        runtime_input_menu_visible(&mut pty, &["Steer #1", "Queued #2"]);
        pty.resize(30, 100);
        runtime_input_menu_visible(&mut pty, &["Steer #1", "Queued #2"]);
        pty.input("\x1b");
        runtime_input_menu_closed(&mut pty);
        runtime_input_draft_visible(&mut pty, &["resize草稿"]);
        runtime_input_cursor_kept(&mut pty, "resize草稿", column);
        pty.text("runtime-live-marker");
        runtime_input_counts(&mut pty, 1, 1, false);
        no_more(&server);

        runtime_input_end_live(stream, protocol);
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["resize-task-U", "steer-item"],
            "{protocol}: steering stays ahead of the follow-up"
        );
        runtime_input_final(request, protocol, "resize-steer-final");
        pty.text("resize-steer-final");

        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["resize-task-U", "steer-item", "queued-item"],
            "{protocol}: follow-up runs after the steering task"
        );
        runtime_input_final(request, protocol, "resize-queued-final");
        pty.text("resize-queued-final");
        runtime_input_queue_drained(&mut pty);
        runtime_input_draft_visible(&mut pty, &["resize草稿"]);
        runtime_input_cursor_kept(&mut pty, "resize草稿", column);

        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}

// 场景 4 补充：Esc 与已提交最终回复的竞争。以 journal 文件确认回复已提交（终局
// 为 Completed）后再按 Esc；暂停意图必须让积压 Steering 留在队列，不自动发请求，
// 功能键 R 才执行它。
#[test]
fn tui_runtime_input_esc_committed_reply_race_keeps_queue_paused() {
    for protocol in ["completion", "response"] {
        let server = server();
        let (home, mut pty) = runtime_input_start(&server, protocol, "");
        pty.input("race-task-U\r");
        let (request, _) = pty.request_at(&server, api_path(protocol));
        runtime_input_calls(
            request,
            protocol,
            &[(
                "race-background",
                "bash",
                json!({
                    "mode":"background",
                    "command":"trap 'printf cleanup > race-cleanup; while [ ! -f race-release ]; do sleep 0.01; done; exit 0' TERM; printf ready > race-ready; while :; do sleep 0.01; done"
                }),
            )],
        );
        let (request, _) = pty.request_at(&server, api_path(protocol));
        cancel_wait_pid_files(home.path(), &["race-ready"]);
        pty.input("race-steer\x1b\r");
        runtime_input_counts(&mut pty, 0, 1, false);
        runtime_input_final(request, protocol, "race-final-marker");
        // Cleanup starts only after the final native reply was committed and
        // steering was atomically closed. The worker cannot emit Completed yet.
        cancel_wait_pid_files(home.path(), &["race-cleanup"]);
        pty.input("\x1b");
        pty.text("Cancelling");
        std::fs::write(home.path().join("race-release"), "release").unwrap();

        pty.wait_for("committed reply still completes the task", |screen| {
            screen.contents().contains("✻ Completed")
        });
        runtime_input_counts(&mut pty, 0, 1, true);
        no_more(&server);

        pty.function_key('r');
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["race-task-U", "race-steer"],
            "{protocol}: Esc before the Completed event must keep the steering \
             message pending until explicit queue resume"
        );
        runtime_input_final(request, protocol, "race-resumed-final");
        pty.text("race-resumed-final");
        runtime_input_queue_drained(&mut pty);

        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}
