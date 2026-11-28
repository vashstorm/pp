// —— 验证项：草稿/光标/粘贴保留，Esc 取消跳过同批 write 且历史成对 ——

#[test]
fn ask_tui_esc_cancel_keeps_draft_skips_batch_write_and_pairs_history() {
    let protocol = "response";
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = ask_tui_start(&server, home.path(), project.path(), protocol);
        pty.ready();
        pty.input("ask esc task\r");
        // 先抓住请求但不回复：worker 等待 SSE，此时输入的草稿必须保留。
        let (request, _) = pty.request_at(&server, api_path(protocol));
        pty.input("排队草稿");
        pty.draft("排队草稿");
        ask_reply_calls(
            request,
            protocol,
            json!([
                ask_call_json(protocol, "ask-esc", ask_single_arguments()),
                ask_call_json(
                    protocol,
                    "ask-esc-write",
                    json!({"path":"later.txt","content":"x"})
                )
            ]),
        );
        ask_wait_panel(&mut pty, "Which database should the project use?");
        pty.draft("排队草稿");
        // 面板内的粘贴不进入主草稿；取消完成后再断言其从未出现。
        pty.input("\x1b[200~粘贴拦截\x1b[201~");
        pty.input("\x1b");
        cancel_wait_cancelled_ready(&mut pty);
        pty.draft("排队草稿");
        ask_assert_cursor_at_draft_end(&pty);
        {
            let text = pty.parser.screen().contents();
            assert!(
                !text.contains("粘贴拦截"),
                "paste inside the panel must not reach the main draft\n{text}"
            );
        }
        assert!(
            std::fs::read(project.path().join("later.txt")).is_err(),
            "cancelled batch must not execute the later write"
        );
        // 取消本身不触发任何 HTTP。
        cancel_assert_no_request(&server);

        // 下一个任务正常提交；历史中两个 call id 与结果成对且顺序稳定。
        ask_clear_draft(&mut pty);
        pty.input("after esc task\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["ask esc task", "after esc task"],
            "protocol {protocol}"
        );
        assert_eq!(
            cancel_tool_outputs(&body, protocol),
            vec![
                (
                    "ask-esc".to_owned(),
                    json!({"ok":false,"cancelled":true,
                           "error":"Tool cancelled; side effects may have occurred."})
                ),
                (
                    "ask-esc-write".to_owned(),
                    json!({"ok":false,"cancelled":true,
                           "error":"Not executed: task cancelled."})
                )
            ]
        );
        assert!(
            std::fs::read(project.path().join("later.txt")).is_err(),
            "write must stay skipped on the next request too"
        );
        ask_final_reply(request, protocol, "after esc done");
        pty.text("after esc done");
        pty.ready();
        pty.input("\x03");
        pty.finish();
        no_more(&server);
}

// —— 验证项：等待 ask 时 Ctrl-C 立即退出并恢复终端 ——

// —— 验证项：/new、跨协议 /model、-r 恢复后 TUI schema 仍含 ask ——

fn ask_schema_offered_tui(body: &Value, protocol: &str) -> bool {
    body["tools"].as_array().is_some_and(|tools| {
        tools.iter().any(|tool| {
            if protocol == "completion" {
                tool["function"]["name"] == "ask"
            } else {
                tool["name"] == "ask"
            }
        })
    })
}

// —— 验证项：replay 只还原问答卡片，不重开交互，答案不进输入历史 ——

// —— 验证项：只有 ToolStarted 没有结果的日志恢复为中断提示，不重开面板 ——

