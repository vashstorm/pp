#[test]
fn slow_ask_tui_provider_argument_variants_succeed_without_model_retry() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = ask_tui_start(&server, home.path(), project.path(), protocol);
        pty.ready();
        let cases = [
            // 最新实际故障：选项级布尔推荐，不需要模型修正后再调用。
            (json!({"options":[{"label":"拆成两个提交","recommended":true},
                {"label":"合并为一个提交"},{"label":"取消提交"}]}), "拆成两个提交", false),
            // 上次实际故障：嵌套数字按数值指向第三项，不能把所在的第一项当作推荐。
            (json!({"options":[{"label":"拆成两个提交","recommended":2},
                {"label":"合并为一个提交"},{"label":"取消提交"}]}), "取消提交", false),
            (json!({"recommended":1,"options":[{"label":"拆成两个提交"},
                {"label":"合并为一个提交","recommended":true},{"label":"取消提交"}]}), "合并为一个提交", false),
            (json!({"recommended":"1","multi":null,
                "options":["拆成两个提交","合并为一个提交","取消提交"]}), "合并为一个提交", false),
            (json!({"options":[{"label":"拆成两个提交","recommended":false},
                {"label":"合并为一个提交","recommended":"1"},{"label":"取消提交"}]}), "合并为一个提交", true),
        ];
        for fragmented in [false, true] {
            for (index, (mut question, expected, encoded)) in cases.clone().into_iter().enumerate() {
                let id = format!("ask-provider-{}-{index}", usize::from(fragmented));
                question["id"] = json!("scope");
                question["question"] = json!("本次变更如何组织提交？");
                let mut arguments = json!({"questions":[question]}).to_string();
                if encoded {
                    arguments = serde_json::to_string(&arguments).unwrap();
                }
                pty.input(&format!("{id} task\r"));
                let (request, _) = pty.request_at(&server, api_path(protocol));
                ask_provider_reply(request, protocol, &id, &arguments, fragmented);
                ask_wait_panel(&mut pty, "本次变更如何组织提交？");
                pty.wait_for("兼容参数推荐项正常显示", |screen| {
                    screen.contents().contains(&format!("{expected} (Recommended)"))
                });
                no_more(&server);
                pty.input("\r");
                let (request, body) = pty.request_at(&server, api_path(protocol));
                let outputs = cancel_tool_outputs(&body, protocol);
                let (_, answer) = outputs.iter().find(|(call_id, _)| call_id == &id).unwrap();
                assert_eq!(answer, &json!({"ok":true,"answers":[{
                    "id":"scope", "selected_options":[expected], "custom_input":null
                }]}), "{protocol}, fragmented={fragmented}, case={index}");
                // 工具执行和恢复可以归一化显示，但发回 provider 的调用参数必须保留原样。
                assert!(body.to_string().contains(&serde_json::to_string(&arguments).unwrap()), "{body}");
                let done = format!("{id} done");
                ask_final_reply(request, protocol, &done);
                pty.text(&done);
                pty.ready();
            }
        }
        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn ask_tui_invalid_arguments_never_open_panel_and_recover() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = ask_tui_start(&server, home.path(), project.path(), protocol);
        pty.ready();
        pty.input("ask invalid task\r");
        let (request, _) = pty.request_at(&server, api_path(protocol));
        ask_reply_calls(
            request,
            protocol,
            json!([
                ask_call_raw(protocol, "ask-json", "not-json"),
                ask_call_raw(protocol, "ask-empty", r#"{"questions":[]}"#),
                ask_call_raw(
                    protocol,
                    "ask-dup",
                    r#"{"questions":[{"id":"dup","question":"Same?","options":[{"label":"a"}]},
                                      {"id":"dup","question":"Again?","options":[{"label":"b"}]}]}"#
                ),
                ask_call_raw(
                    protocol,
                    "ask-range",
                    r#"{"questions":[{"id":"range","question":"Pick?","options":[{"label":"only"}],"recommended":9}]}"#
                ),
                ask_call_raw(
                    protocol,
                    "ask-blank",
                    r#"{"questions":[{"id":"blank","question":"t","options":[{"label":" "}]}]}"#
                )
            ]),
        );
        // 五个调用全部快速失败：错误卡片可见，问题文本与面板入口从不出现。
        pty.wait_for("invalid ask errors surfaced", |screen| {
            let text = screen.contents();
            text.contains("invalid ask arguments")
                && text.contains("ask requires at least one question")
                && text.contains("duplicate ask question id")
                && text.contains("ask recommended index is out of range")
                && text.contains("ask option label must not be blank")
        });
        {
            let text = pty.parser.screen().contents();
            assert!(
                !text.contains("Other (type your own)")
                    && !text.contains("Same?")
                    && !text.contains("Pick?"),
                "malformed ask must not open the panel\n{text}"
            );
        }
        let (request, body) = pty.request_at(&server, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(outputs.len(), 5, "protocol {protocol}: {outputs:?}");
        for (id, message) in [
            ("ask-json", "invalid ask arguments"),
            ("ask-empty", "ask requires at least one question"),
            ("ask-dup", "duplicate ask question id"),
            ("ask-range", "ask recommended index is out of range"),
            ("ask-blank", "ask option label must not be blank"),
        ] {
            let (_, value) = outputs.iter().find(|(call, _)| call == id).unwrap();
            assert_eq!(value["ok"], false);
            assert_eq!(value["error"], message);
        }
        ask_final_reply(request, protocol, "recovered after invalid ask");
        pty.text("recovered after invalid ask");
        pty.ready();

        // 不挂起：下一个任务照常提交并得到独立回合。
        pty.input("ask recovery task\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["ask invalid task", "ask recovery task"]
        );
        ask_final_reply(request, protocol, "recovery task done");
        pty.text("recovery task done");
        pty.ready();
        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}

// —— 验证项：草稿/光标/粘贴保留，Esc 取消跳过同批 write 且历史成对 ——

#[test]
fn slow_ask_tui_esc_cancel_keeps_draft_skips_batch_write_and_pairs_history() {
    for protocol in ["completion", "response"] {
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
}

// —— 验证项：等待 ask 时 Ctrl-C 立即退出并恢复终端 ——

#[test]
fn ask_tui_ctrl_c_while_waiting_restores_terminal() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = ask_tui_start(&server, home.path(), project.path(), "completion");
    pty.ready();
    pty.input("ask ctrl-c task\r");
    let (request, _) = pty.request_at(&server, "/v1/chat/completions");
    ask_reply_calls(
        request,
        "completion",
        json!([ask_call_json("completion", "ask-cc", ask_single_arguments())]),
    );
    ask_wait_panel(&mut pty, "Which database should the project use?");
    pty.input("\x03");
    // finish() 断言五秒内退出、退出码 0、alternate screen/bracketed paste/
    // 鼠标捕获与 termios 全部还原：不因 reply channel 阻塞。
    pty.finish();
    no_more(&server);
}

// —— 验证项：/new、跨协议 /model、-r 恢复后 TUI schema 仍含 ask ——

#[test]
fn ask_tui_schema_survives_new_model_switch_and_resume() {
    let server = server();
    // 第一段：同一 TUI 内 /new 与跨协议 /model。
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let models = json!({"providers": {
        "fixture": slash_provider(&server, "fixture", "completion", json!({
            "primary": {"model":"vendor-a"}})),
        "gateway": slash_provider(&server, "gateway", "response", json!({
            "secondary": {"model":"vendor-b"}}))}})
    .to_string();
    crate::write_settings(home.path(), fixture_settings()).unwrap();
    std::fs::write(config_path(home.path(), "models.yml"), models).unwrap();
    let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
    pty.ready();
    pty.input("ask schema task one\r");
    let (request, body) = pty.request_at(&server, "/v1/chat/completions");
    assert!(ask_schema_offered_tui(&body, "completion"), "{body}");
    final_reply(request, "schema answer one");
    pty.text("schema answer one");
    pty.completed();
    pty.input("/new\r");
    pty.wait_for("reset clears the transcript", |screen| {
        input_is_ready(screen) && !screen.contents().contains("schema answer one")
    });
    pty.input("ask schema task two\r");
    let (request, body) = pty.request_at(&server, "/v1/chat/completions");
    assert!(ask_schema_offered_tui(&body, "completion"), "{body}");
    final_reply(request, "schema answer two");
    pty.text("schema answer two");
    pty.completed();
    pty.input("/model gateway/secondary\r");
    pty.ready_model("gateway/secondary");
    pty.input("ask schema task three\r");
    let (request, body) = pty.request_at(&server, "/v1/responses");
    assert!(ask_schema_offered_tui(&body, "response"), "{body}");
    response_final(request, "schema answer three");
    pty.text("schema answer three");
    pty.completed();
    pty.input("\x03");
    pty.finish();
    no_more(&server);

    // 第二段：独立 HOME 建会话后用 -r 启动恢复，schema 仍在。
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut saved = ask_tui_start(&server, home.path(), project.path(), "response");
    saved.ready();
    saved.input("ask resume seed task\r");
    let (request, body) = saved.request_at(&server, "/v1/responses");
    assert!(ask_schema_offered_tui(&body, "response"), "{body}");
    response_final(request, "resume seed answer");
    saved.text("resume seed answer");
    saved.completed();
    saved.input("\x03");
    saved.finish();
    let id = saved_session_id(home.path());
    let mut resumed = Pty::start_with_args(project.path(), home.path(), &["-r", &id]);
    resumed.wait_for("startup resume replays the seed answer", |screen| {
        input_is_ready(screen)
            && screen.contents().contains("resume seed answer")
    });
    resumed.input("ask after resume task\r");
    let (request, body) = resumed.request_at(&server, "/v1/responses");
    assert!(ask_schema_offered_tui(&body, "response"), "{body}");
    response_final(request, "after resume answer");
    resumed.text("after resume answer");
    resumed.completed();
    resumed.input("\x03");
    resumed.finish();
    no_more(&server);
}

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

#[test]
fn ask_tui_replay_restores_cards_without_reopening() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = ask_tui_start(&server, home.path(), project.path(), protocol);
        pty.ready();
        pty.input("ask replay task\r");
        let (request, _) = pty.request_at(&server, api_path(protocol));
        ask_reply_calls(
            request,
            protocol,
            json!([ask_call_json(
                protocol,
                "ask-replay",
                json!({"questions":[{"id":"db","header":"Database","question":"Pick one?",
                    "options":[{"label":"MySQL","description":"mysql-details"},
                        {"label":"Postgres","description":"postgres-details","preview":"**Replay preview**","recommended":true}]}]})
            )]),
        );
        ask_wait_panel(&mut pty, "Pick one?");
        // 单题单选：Enter 选中并立即提交。
        pty.input("\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_tool_outputs(&body, protocol),
            vec![(
                "ask-replay".to_owned(),
                json!({"ok":true,"answers":[
                    {"id":"db","selected_options":["Postgres"],"custom_input":null}
                ]})
            )],
            "protocol {protocol}"
        );
        ask_final_reply(request, protocol, "replay answer final");
        pty.text("replay answer final");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
        let id = saved_session_id(home.path());

        let mut resumed = Pty::start_with_args(project.path(), home.path(), &["-r", &id]);
        resumed.ready();
        resumed.wait_for("replayed ask card shows the answers", |screen| {
            let text = screen.contents();
            text.contains("Database · Pick one?") && text.contains("󰐾 Postgres")
                && text.contains("mysql-details") && text.contains("postgres-details")
                && text.contains("Replay preview")
        });
        {
            let text = resumed.parser.screen().contents();
            assert!(
                !text.contains("Other (type your own)"),
                "resume must not reopen the answered panel\n{text}"
            );
        }
        // 答案不进入主输入历史：↑ 是原用户输入而不是选项 label。
        resumed.input("\x1b[A");
        resumed.draft("ask replay task");
        ask_clear_draft(&mut resumed);
        resumed.input("after replay task\r");
        let (request, body) = resumed.request_at(&server, api_path(protocol));
        assert!(
            cancel_tool_outputs(&body, protocol)
                .iter()
                .any(|(id, value)| id == "ask-replay" && value["ok"] == true),
            "asked answers must stay paired in the resumed history: {body}"
        );
        ask_final_reply(request, protocol, "after replay answer");
        resumed.text("after replay answer");
        resumed.ready();
        resumed.input("\x03");
        resumed.finish();
        no_more(&server);
    }
}

// —— 验证项：只有 ToolStarted 没有结果的日志恢复为中断提示，不重开面板 ——

#[test]
fn ask_tui_interrupted_ask_replays_without_reopening() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = ask_tui_start(&server, home.path(), project.path(), "completion");
    pty.ready();
    pty.input("ask interrupted task\r");
    let (request, _) = pty.request_at(&server, "/v1/chat/completions");
    ask_reply_calls(
        request,
        "completion",
        json!([ask_call_json("completion", "ask-int", ask_single_arguments())]),
    );
    ask_wait_panel(&mut pty, "Which database should the project use?");
    // 等待回答时直接杀死进程：日志里留下没有结果的 ask 调用。
    drop(pty);
    let id = saved_session_id(home.path());
    let mut resumed = Pty::start_with_args(project.path(), home.path(), &["-r", &id]);
    resumed.wait_for("interrupted ask reported", |screen| {
        let text = screen.contents();
        text.contains("Previous request was interrupted.")
            && !text.contains("Other (type your own)")
    });
    assert!(
        input_is_ready(resumed.parser.screen()),
        "{}",
        resumed.diagnostics()
    );
    resumed.input("post interrupted task\r");
    let (request, body) = resumed.request_at(&server, "/v1/chat/completions");
    // 被中断的 ask 不得留下未配对的工具调用。
    let called = cancel_called_ids(&body, "completion");
    let outputs = cancel_tool_outputs(&body, "completion");
    for id in &called {
        assert!(
            outputs.iter().any(|(output, _)| output == id),
            "unpaired interrupted call {id}: {body}"
        );
    }
    final_reply(request, "post interrupted answer");
    resumed.text("post interrupted answer");
    resumed.ready();
    resumed.input("\x03");
    resumed.finish();
    no_more(&server);
}
