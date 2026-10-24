// 运行中输入的恢复与额度/压缩边界（计划验证项 6、7）：真实 pp 二进制 PTY 场景。
// 场景 6：已应用 Steering + 真实 write 副作用的任务完成后，再产生已暂停的未应用
// 队列与未提交草稿并退出；-r 恢复只回放已应用历史各一份，不恢复内存队列，
// 新任务请求恰好包含旧 Steering 一次且不含未应用消息与草稿，旧 write 副作用
// 不重做（请求只回放旧工具结果）。场景 7a：max_model_requests=1 时首轮完成后
// 运行中排入的 Steering 触发额度错误并暂停保留，功能键 R 之前不发第二个 HTTP，
// 恢复后才以新任务执行该 Steering。场景 7b：显式 context_window 的自动摘要被
// 服务端握住时排入 Steering；摘要请求不含它，摘要后的普通请求包含它且工具
// 批次继续完整，write 副作用真实落盘。全部用真实终端、loopback HTTP 与文件
// marker 作栅栏，不用 sleep 推测阶段；辅助命名沿用 runtime_input_ 前缀且与
// 兄弟文件不冲突。

#[test]
fn tui_runtime_input_quota_holds_pending_until_resume_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = runtime_input_start(&fixture, protocol, "max_model_requests: 1");
        let stream = runtime_input_live(&mut pty, &fixture, protocol, "quota-live-prompt");
        // SSE 保持打开期间排入 Steering：已计数、未暂停、草稿清空、无第二个请求。
        pty.input("quota-steer\r");
        runtime_input_counts(&mut pty, 1, 0, false);
        pty.draft("");
        no_more(&fixture);
        runtime_input_end_live(stream, protocol);
        // 首轮完成后的边界领取命中额度：错误终态、队列保留并暂停。
        pty.wait_for("额度错误终态与 Failed 活动", |screen| {
            let text = screen.contents();
            input_is_ready(screen)
                && text.contains("✻ Failed")
                && text.contains("1 model request limit reached")
        });
        runtime_input_counts(&mut pty, 1, 0, true);
        no_more(&fixture);
        // 功能键 R 显式恢复：Steering 以新任务执行，历史恰好 [U, S]。
        pty.function_key('r');
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            vec!["quota-live-prompt".to_owned(), "quota-steer".to_owned()]
        );
        assert!(body["tools"].as_array().is_some());
        runtime_input_final(request, protocol, "quota-resumed-answer");
        pty.text("quota-resumed-answer");
        pty.ready();
        // 队列已清空：计数行不再出现。
        pty.wait_for("恢复后队列计数消失", |screen| {
            !screen.contents().contains("Steering:")
        });
        no_more(&fixture);
        pty.input("\x03");
        pty.finish();
    }
}

#[test]
fn tui_runtime_input_resume_replays_applied_history_once_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (home, mut pty) = runtime_input_start(&fixture, protocol, "");
        // 任务 1：U 运行中排入 S，结束后 S 与 U 同请求，模型调用 write 落盘。
        let stream = runtime_input_live(&mut pty, &fixture, protocol, "resume-live-prompt");
        pty.input("resume-steer\r");
        runtime_input_counts(&mut pty, 1, 0, false);
        pty.draft("");
        runtime_input_end_live(stream, protocol);
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            vec!["resume-live-prompt".to_owned(), "resume-steer".to_owned()]
        );
        runtime_input_calls(
            request,
            protocol,
            &[(
                "resume-write-1",
                "write",
                json!({"path":"resume-proof.txt","content":"resume-ok\n"}),
            )],
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].0, "resume-write-1");
        runtime_input_final(request, protocol, "resume-task-answer");
        pty.text("resume-task-answer");
        pty.ready();
        assert_eq!(
            std::fs::read_to_string(home.path().join("resume-proof.txt")).unwrap(),
            "resume-ok\n"
        );
        // 任务 2：运行中 Alt+Enter 排入未应用 Q1，Esc 取消后队列暂停，保留草稿退出。
        let stream = runtime_input_live(&mut pty, &fixture, protocol, "resume-cancel-prompt");
        pty.input("resume-queued\x1b\r");
        runtime_input_counts(&mut pty, 0, 1, false);
        pty.draft("");
        pty.input("\x1b");
        cancel_wait_cancelled_ready(&mut pty);
        drop(stream);
        runtime_input_counts(&mut pty, 0, 1, true);
        pty.input("resume-draft");
        pty.draft("resume-draft");
        pty.input("\x03");
        pty.finish();

        // A changed sentinel detects even an accidental replay of the same write.
        std::fs::write(
            home.path().join("resume-proof.txt"),
            "must-not-be-overwritten\n",
        )
        .unwrap();
        // -r 恢复：只回放已应用历史，不发请求，也不恢复内存队列与草稿。
        let id = saved_session_id(home.path());
        let mut resumed = Pty::start_with_args(home.path(), home.path(), &["-r", &id]);
        resume_history_wait_resumed(&mut resumed, &id, &[]);
        let replayed = resume_history_collect_order(
            &mut resumed,
            &[
                "resume-live-prompt",
                "resume-steer",
                "resume-task-answer",
                "resume-cancel-prompt",
            ],
        );
        assert_eq!(
            replayed,
            vec![
                "resume-live-prompt".to_owned(),
                "resume-steer".to_owned(),
                "resume-task-answer".to_owned(),
                "resume-cancel-prompt".to_owned(),
            ]
        );
        context_tui_page_to_bottom(&mut resumed);
        let visible = resumed.parser.screen().contents();
        assert!(!visible.contains("resume-queued"), "{visible}");
        assert!(!visible.contains("resume-draft"), "{visible}");
        assert!(!visible.contains("Steering:"), "{visible}");
        no_more(&fixture);

        // 显式新任务：原生历史 S 恰好一次，未应用消息与草稿不出现；
        // 旧 write 结果只被回放，副作用不重做，文件内容保持原样。
        resumed.input("resume-final-prompt\r");
        let (request, body) = resumed.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            vec![
                "resume-live-prompt".to_owned(),
                "resume-steer".to_owned(),
                "resume-cancel-prompt".to_owned(),
                "resume-final-prompt".to_owned(),
            ]
        );
        assert!(
            !serde_json::to_string(&body)
                .unwrap()
                .contains("resume-draft")
        );
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].0, "resume-write-1");
        runtime_input_final(request, protocol, "resume-final-answer");
        resumed.text("resume-final-answer");
        resumed.ready();
        no_more(&fixture);
        resumed.input("\x03");
        resumed.finish();
        assert_eq!(
            std::fs::read_to_string(home.path().join("resume-proof.txt")).unwrap(),
            "must-not-be-overwritten\n"
        );
    }
}

#[test]
fn tui_runtime_input_auto_compact_excludes_held_steering_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (home, mut pty) = context_tui_start_auto(&fixture, protocol, 32768, 80);
        let old = "runtime-compact-old";
        let recent = "runtime-compact-recent";
        context_tui_write_batch(
            home.path(),
            "runtime-compact-old.txt",
            old,
            CONTEXT_TUI_AUTO_FIRST_BYTES,
        );
        context_tui_write_batch(
            home.path(),
            "runtime-compact-new.txt",
            recent,
            CONTEXT_TUI_AUTO_SECOND_BYTES,
        );
        pty.input("runtime compact prompt\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(body["tools"].as_array().is_some());
        context_tui_read_calls(
            request,
            protocol,
            "runtime-compact-one",
            "runtime-compact-old.txt",
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(
            body["tools"].as_array().is_some(),
            "首个批次不得提前触发自动摘要"
        );
        context_tui_read_calls(
            request,
            protocol,
            "runtime-compact-two",
            "runtime-compact-new.txt",
        );
        // 第二个批次越过阈值：自动摘要请求被服务端握住，屏幕保持 Compacting。
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        context_tui_assert_summary_body(&body, protocol, old, recent);
        assert!(
            !serde_json::to_string(&body)
                .unwrap()
                .contains("runtime-compact-steer"),
            "摘要请求不得包含尚未排入的补充"
        );
        pty.text("Compacting context");
        // 摘要未完成时排入 Steering：仍计入队列，等摘要后的边界才消费。
        pty.input("runtime-compact-steer\r");
        runtime_input_counts(&mut pty, 1, 0, false);
        pty.draft("");
        no_more(&fixture);
        context_tui_final(request, protocol, CONTEXT_TUI_SUMMARY);
        // 摘要后的普通请求：摘要包装 + 原生近期批次 + Steering 恰好一次。
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let text = serde_json::to_string(&body).unwrap();
        assert!(
            body["tools"].as_array().is_some(),
            "普通请求必须保留工具配置"
        );
        assert_eq!(text.matches("[Context summary]").count(), 1);
        assert!(text.contains(CONTEXT_TUI_SUMMARY));
        assert!(text.contains(recent), "最近完整批次必须原生保留");
        assert!(!text.contains(old), "旧前缀原文不得复活");
        assert_eq!(text.matches("runtime-compact-steer").count(), 1);
        let users = cancel_user_messages(&body, protocol);
        assert_eq!(users.len(), 2);
        assert!(users[0].contains("[Context summary]"));
        assert_eq!(users[1], "runtime-compact-steer");
        assert_eq!(cancel_tool_outputs(&body, protocol).len(), 1);
        // 工具批次继续完整：write 副作用真实落盘后再收尾。
        runtime_input_calls(
            request,
            protocol,
            &[(
                "runtime-compact-write",
                "write",
                json!({"path":"compact-steering-proof.txt","content":"compact-steer-ok\n"}),
            )],
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].0, "runtime-compact-two");
        assert_eq!(outputs[1].0, "runtime-compact-write");
        assert_eq!(cancel_user_messages(&body, protocol), users);
        runtime_input_final(request, protocol, "runtime-compact-answer");
        pty.text("runtime-compact-answer");
        pty.ready();
        assert_eq!(
            std::fs::read_to_string(home.path().join("compact-steering-proof.txt")).unwrap(),
            "compact-steer-ok\n"
        );
        pty.wait_for("Steering 应用后队列计数消失", |screen| {
            !screen.contents().contains("Steering:")
        });
        no_more(&fixture);
        pty.input("\x03");
        pty.finish();
    }
}
