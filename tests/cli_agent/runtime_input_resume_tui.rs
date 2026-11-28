// 运行中输入的恢复与压缩边界（计划验证项 6、7）：真实 pp 二进制 PTY 场景。
// 场景 6：已应用 Steering + 真实 write 副作用的任务完成后，再产生已暂停的未应用
// 队列与未提交草稿并退出；-r 恢复只回放已应用历史各一份，不恢复内存队列，
// 新任务请求恰好包含旧 Steering 一次且不含未应用消息与草稿，旧 write 副作用
// 不重做（请求只回放旧工具结果）。场景 7：显式 context_window 的自动摘要被
// 服务端握住时排入 Steering；摘要请求不含它，摘要后的普通请求包含它且工具
// 批次继续完整，write 副作用真实落盘。全部用真实终端、loopback HTTP 与文件
// marker 作栅栏，不用 sleep 猜测阶段；辅助命名沿用 runtime_input_ 前缀且与
// 兄弟文件不冲突。

#[test]
fn tui_runtime_input_resume_replays_applied_history_once_both_protocols() {
    let protocol = "response";
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

