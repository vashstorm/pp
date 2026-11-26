// ask 端到端 TUI 回归（计划 Verification 的 ask 各行）：多题 Review 修改与
// 精确答案、空选项自由回答与空多选提交、畸形参数不开面板、Esc 取消保留草稿
// 并跳过同批 write、等待时 Ctrl-C 退出、/new 与跨协议 /model 与 -r 恢复后
// schema 仍在、replay 只还原问答卡片。全部通过真实 PTY、loopback HTTP 与
// 屏幕/HTTP 栅栏观察，不用 sleep 猜阶段。辅助项均以 `ask_` 前缀命名，复用
// 同作用域内 cancel_/resume_ 既有 harness（cancel_tool_outputs、
// cancel_wait_cancelled_ready、saved_session_id、api_path、notification_bounds）。

// —— 共享 fixture ——

// 两题批次：storage 单选（recommended=0 是 SQLite）、checks 多选。
fn ask_batch_arguments() -> Value {
    json!({"questions":[
        {"id":"storage","header":"Storage","question":"Which database should the project use?",
         "options":[{"label":"SQLite"},{"label":"Postgres"}],"recommended":0},
        {"id":"checks","header":"Checks","question":"Which checks should run?",
         "multi":true,"options":[{"label":"unit"},{"label":"lint"}]}
    ]})
}

fn ask_single_arguments() -> Value {
    json!({"questions":[
        {"id":"storage","question":"Which database should the project use?",
         "options":[{"label":"SQLite"},{"label":"Postgres"}],"recommended":0}
    ]})
}

// 真实终端中同时切换多个已完成工具，并在 ask 持有焦点时保留选择和主草稿。
#[test]
fn tool_folding_ctrl_o_globally_toggles_during_ask_and_new_tools_inherit() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        for name in ["first", "second", "third"] {
            let body = (0..20).map(|i| format!("fold-{name}-{i:02}"))
                .collect::<Vec<_>>().join("\n");
            std::fs::write(project.path().join(name), body).unwrap();
        }
        let mut pty = ask_tui_start(&server, home.path(), project.path(), protocol);
        pty.ready();
        pty.resize(110, 100);
        pty.input("global tool folding\r");
        let (request, _) = pty.request_at(&server, api_path(protocol));
        pty.input("保留草稿");
        pty.draft("保留草稿");
        let mk_call = |id: &str, name: &str, args| {
            if protocol == "completion" { call(id, name, args) }
            else { response_call(id, name, args) }
        };
        ask_reply_calls(request, protocol, json!([
            mk_call("first", "read", json!({"path":"first"})),
            mk_call("second", "read", json!({"path":"second"})),
            mk_call("bash", "bash", json!({"command":"for i in {0..19}; do printf 'fold-bash-%02d\\n' \"$i\"; done"})),
            ask_call_json(protocol, "fold-ask", ask_single_arguments())
        ]));
        ask_wait_panel(&mut pty, "Which database should the project use?");
        pty.wait_for("read 摘要和 bash 十行尾部", |screen| {
            let text = screen.contents();
            text.matches("Ctrl + O to expand").count() == 1
                && text.contains("Read (2)")
                && text.contains("fold-bash-10") && !text.contains("fold-bash-00")
                && !text.contains("fold-first-") && !text.contains("fold-second-")
        });
        pty.input("\x1b[B ");
        pty.wait_for("选择答案后保持面板", |screen| screen.contents().contains("󰐾 Postgres"));
        pty.input("\x0f");
        pty.wait_for("ask 焦点下全局展开", |screen| {
            let text = screen.contents();
            !text.contains("fold-first-") && !text.contains("fold-second-")
                && text.contains("fold-bash-00") && !text.contains("to expand")
                && text.contains("󰐾 Postgres") && text.contains("保留草稿")
        });
        pty.input("\x0f");
        pty.wait_for("ask 焦点下全局收起", |screen| {
            let text = screen.contents();
            text.matches("Ctrl + O to expand").count() == 1
                && !text.contains("fold-first-19") && !text.contains("fold-second-19")
                && !text.contains("fold-bash-00") && text.contains("󰐾 Postgres")
        });
        pty.input("\x0f");
        pty.text("fold-bash-00");
        pty.input("\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(outputs.len(), 4);
        assert!(outputs[0].1["content"].as_str().unwrap().contains("fold-first-19"));
        assert!(outputs[1].1["content"].as_str().unwrap().contains("fold-second-19"));
        assert!(outputs[2].1["output"].as_str().unwrap().contains("fold-bash-00"));
        assert_eq!(outputs[3].1["answers"][0]["selected_options"], json!(["Postgres"]));
        ask_reply_calls(request, protocol, json!([
            mk_call("third", "read", json!({"path":"third"}))
        ]));
        let (request, _) = pty.request_at(&server, api_path(protocol));
        pty.wait_for("后续 read 仍只显示摘要", |screen| {
            let text = screen.contents();
            text.contains("Read · third") && !text.contains("fold-third-") && !text.contains("to expand")
        });
        ask_final_reply(request, protocol, "global folding done");
        pty.text("global folding done");
        pty.ready();
        pty.draft("保留草稿");
        ask_assert_cursor_at_draft_end(&pty);
        pty.input("\x0f");
        pty.wait_for("任务结束后仍可全局折叠", |screen| {
            screen.contents().matches("Ctrl + O to expand").count() == 1
        });
        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}

// 摘要不影响模型正文；恢复、预览配置和原生历史均不显示 read 正文。
#[test]
fn tool_folding_omp_read_default_summary_resume_and_native_scrollback() {
    for protocol in ["completion", "response"] {
        let provider = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        crate::write_settings(home.path(), r#"{"notification_timeout_seconds":1}"#).unwrap();
        std::fs::write(config_path(home.path(), "models.yml"), response_models(&provider, Some(protocol))).unwrap();
        for name in ["one", "two"] {
            std::fs::write(project.path().join(name), (0..20).map(|i|format!("body-{name}-{i:02}\n")).collect::<String>()).unwrap();
        }
        let mut pty = Pty::start_with_args(project.path(), home.path(), &["--yolo"]);
        pty.ready();
        pty.resize(80, 100);
        pty.input("read summary\r");
        let (request, _) = pty.request_at(&provider, api_path(protocol));
        ask_reply_calls(request, protocol, json!([
            diff_blocks_call(protocol,"one","read",json!({"path":"one"})),
            diff_blocks_call(protocol,"two","read",json!({"path":"two:2"}))
        ]));
        let (request, body) = pty.request_at(&provider, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        assert!(outputs[0].1["content"].as_str().unwrap().contains("body-one-19"));
        assert!(outputs[1].1["content"].as_str().unwrap().starts_with("body-two-01"));
        pty.wait_for("默认 read 本地摘要", |screen| {
            let text = screen.contents();
            text.contains("Read (2)") && text.contains("one:1-20") && text.contains("two:2-20") && !text.contains("body-one-")
        });
        {
            let screen = pty.parser.screen();
            for (label, color) in [
                ("\u{f00c}", vt100::Color::Rgb(137, 210, 129)),
                ("Read", vt100::Color::Rgb(145, 183, 218)),
                ("(2)", vt100::Color::Rgb(96, 102, 114)),
                ("├─", vt100::Color::Rgb(96, 102, 114)),
                ("one", vt100::Color::Rgb(254, 188, 56)),
                (":1-20", vt100::Color::Rgb(190, 198, 212)),
                ("two", vt100::Color::Rgb(254, 188, 56)),
                (":2-20", vt100::Color::Rgb(190, 198, 212)),
            ] {
                let (row, col) = ask_panel_find(screen, label);
                let cell = screen.cell(row, col).unwrap();
                assert_eq!(cell.fgcolor(), color, "{label}");
                assert_eq!(cell.bgcolor(), vt100::Color::Default, "{label}");
                assert!(!cell.bold(), "{label}");
            }
            assert!(!screen.contents().contains(" · ok"));
        }
        pty.input("barrier");
        pty.draft("barrier");
        ask_final_reply(request, protocol, "summary done");
        pty.text("summary done");
        pty.ready();
        let (scrollback, _) = diff_blocks_exit_scrollback(&mut pty, "summary done");
        assert!(scrollback.contains("Read (2)"));
        assert!(!scrollback.contains("body-one-19") && !scrollback.contains("body-two-19"));
        pty.finish();
        let id = saved_session_id(home.path());
        crate::write_settings(home.path(), r#"{"notification_timeout_seconds":1,"read":{"toolResultPreview":true}}"#).unwrap();
        let mut resumed = Pty::start_with_args(project.path(), home.path(), &["-r", &id]);
        resumed.resize(80, 100);
        resumed.ready();
        resumed.wait_for("恢复后仍只显示 read 摘要", |screen| {
            let text = screen.contents();
            text.contains("Read (2)") && !text.contains("body-one-") && !text.contains("Ctrl + O to expand")
        });
        resumed.input("\x0fa");
        resumed.draft("a");
        assert!(!resumed.parser.screen().contents().contains("body-one-"));
        resumed.input("\x03");
        resumed.finish();
        no_more(&provider);
    }
}

#[test]
fn tool_folding_omp_native_search_clips_long_rows_for_both_protocols() {
    for protocol in ["completion", "response"] {
        for name in ["rg", "glob"] {
            let provider = server();
            let home = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            std::fs::create_dir(project.path().join("files")).unwrap();
            let content = (0..20).map(|i| format!("entry-{i:02}-needle-{}\n", "中😀".repeat(40))).collect::<String>();
            std::fs::write(project.path().join("lines"), content).unwrap();
            for i in 0..20 { std::fs::write(project.path().join("files").join(format!("path-{i:02}-{}", "x".repeat(150))), "").unwrap(); }
            let mut pty = ask_tui_start(&provider, home.path(), project.path(), protocol);
            pty.ready();
            pty.resize(90, 80);
            pty.input("search preview\r");
            let (request, body) = pty.request_at(&provider, api_path(protocol));
            assert!(body["tools"].as_array().unwrap().iter().map(|t| if protocol == "completion" { &t["function"] } else { t }).any(|t| t["name"] == name));
            let args = if name == "rg" { json!({"pattern":"needle","path":"lines"}) }
                else { json!({"pattern":"*","path":"files"}) };
            ask_reply_calls(request, protocol, json!([diff_blocks_call(protocol,"search",name,args)]));
            let (request, body) = pty.request_at(&provider, api_path(protocol));
            let outputs = cancel_tool_outputs(&body, protocol);
            assert_eq!(outputs[0].1["ok"], true);
            let content = &outputs[0].1["content"];
            assert!(content.as_str().unwrap().lines().count() >= 20);
            let marker = if name == "rg" { "entry-" } else { "path-" };
            let shown = if name == "rg" { 5 } else { 7 };
            pty.wait_for("搜索正文按额度及列宽收敛", |screen| {
                let text = screen.contents();
                text.matches(marker).count() == shown && text.contains("more") && text.contains("Ctrl + O")
            });
            if name == "rg" {
                let screen = pty.parser.screen();
                let text = screen.contents();
                assert!(text.contains("Grep: needle 20 matches · 1 file · in lines"), "{text}");
                let row = text.lines().position(|line| line.contains("└─ *1│entry-00")).unwrap() as u16;
                let cell = screen.cell(row, 2).unwrap();
                assert_eq!(cell.contents(), "└");
                assert_eq!(cell.fgcolor(), vt100::Color::Rgb(156, 220, 254));
                assert_eq!(cell.bgcolor(), vt100::Color::Rgb(22, 26, 31));
            } else {
                let text = pty.parser.screen().contents();
                assert!(text.contains("Glob: * · files") && text.contains("files/"), "{text}");
                assert!(!text.contains("# files/"), "{text}");
            }
            pty.input("\x0fa");
            pty.draft("a");
            assert_eq!(pty.parser.screen().contents().matches(marker).count(),20);
            ask_final_reply(request, protocol, "search complete");
            pty.text("search complete");
            pty.ready();
            pty.input("\x0fb");
            pty.draft("ab");
            assert_eq!(pty.parser.screen().contents().matches(marker).count(),shown);
            pty.input("\x03");
            pty.finish();
            no_more(&provider);
        }
    }
}

#[test]
fn tool_folding_omp_mcp_multiline_result_and_write_preview() {
    for protocol in ["completion", "response"] {
        let provider = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let content = (0..20).map(|i|format!("mcp-body-{i:02}")).collect::<Vec<_>>().join("\n");
        let encoded = serde_json::to_string(&content).unwrap();
        let mut settings = super::mcp::settings("");
        settings["notification_timeout_seconds"] = json!(1);
        settings["mcpServers"]["local"]["env"]["MCP_VALUE"] = json!(&encoded[1..encoded.len()-1]);
        crate::write_settings(home.path(), settings.to_string()).unwrap();
        std::fs::write(config_path(home.path(), "models.yml"), response_models(&provider, Some(protocol))).unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &["--yolo"]);
        pty.ready();
        pty.resize(90, 100);
        pty.input("mcp and write\r");
        let (request, _) = pty.request_at(&provider, api_path(protocol));
        let written = (0..20).map(|i|format!("write-body-{i:02}")).collect::<Vec<_>>().join("\n");
        ask_reply_calls(request, protocol, json!([
            diff_blocks_call(protocol,"mcp","mcp__local__echo",json!({"text":"test"})),
            diff_blocks_call(protocol,"write","write",json!({"path":"written","content":written}))
        ]));
        let (request, body) = pty.request_at(&provider, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(outputs[0].1["result"]["content"][0]["text"],content);
        assert_eq!(std::fs::read_to_string(project.path().join("written")).unwrap(),written);
        pty.wait_for("MCP 四行与 write 末尾十六行预览", |screen| {
            let text = screen.contents();
            text.contains("mcp-body-03") && !text.contains("mcp-body-04")
                && text.contains("write-body-04") && text.contains("write-body-19")
                && !text.contains("write-body-03")
        });
        pty.input("\x0fa"); pty.draft("a");
        let expanded = pty.parser.screen().contents();
        assert!(expanded.contains("mcp-body-11") && !expanded.contains("mcp-body-12"));
        assert!(expanded.contains("write-body-00") && expanded.contains("write-body-19"));
        ask_final_reply(request, protocol, "mcp write done");
        pty.text("mcp write done"); pty.ready();
        let (scrollback, _) = diff_blocks_exit_scrollback(&mut pty, "mcp write done");
        assert!(scrollback.contains("mcp-body-11") && scrollback.contains("write-body-19"));
        assert!(!scrollback.contains("mcp-body-19"));
        pty.finish();
        no_more(&provider);
        super::mcp::assert_stopped(project.path());
    }
}

fn ask_call_json(protocol: &str, id: &str, arguments: Value) -> Value {
    if protocol == "completion" {
        call(id, "ask", arguments)
    } else {
        response_call(id, "ask", arguments)
    }
}

// arguments 为原始字符串（畸形参数用）。
fn ask_call_raw(protocol: &str, id: &str, arguments: &str) -> Value {
    if protocol == "completion" {
        json!({"id":id,"type":"function","function":{"name":"ask","arguments":arguments}})
    } else {
        json!({
            "type":"function_call","id":format!("fc-{id}"),"call_id":id,
            "name":"ask","arguments":arguments,"status":"completed"
        })
    }
}

fn ask_reply_calls(request: CapturedRequest, protocol: &str, calls: Value) {
    if protocol == "completion" {
        calls_reply(request, calls);
    } else {
        response_reply(request, calls);
    }
}

// 覆盖完整参数快照与分片 SSE；Responses 起始项省略 arguments 的格式也必须可用。
fn ask_provider_reply(request: CapturedRequest, protocol: &str, id: &str, arguments: &str, fragmented: bool) {
    if !fragmented {
        ask_reply_calls(request, protocol, json!([ask_call_raw(protocol, id, arguments)]));
        return;
    }
    let split = arguments.char_indices().nth(arguments.chars().count() / 2).unwrap().0;
    let parts = [&arguments[..split], &arguments[split..]];
    let mut stream = request.start_sse().unwrap();
    if protocol == "completion" {
        stream.event(&chat_chunk(json!({"role":"assistant","tool_calls":[{
            "index":0,"id":id,"type":"function","function":{"name":"ask","arguments":parts[0]}
        }]}), None)).unwrap();
        stream.event(&chat_chunk(json!({"tool_calls":[{
            "index":0,"function":{"arguments":parts[1]}
        }]}), None)).unwrap();
        stream.event(&chat_chunk(json!({}), Some("tool_calls"))).unwrap();
        stream.frame("[DONE]").unwrap();
    } else {
        let item = ask_call_raw(protocol, id, arguments);
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        added.as_object_mut().unwrap().remove("arguments");
        stream.event(&json!({"type":"response.output_item.added","sequence_number":0,
            "output_index":0,"item":added})).unwrap();
        for (index, part) in parts.into_iter().enumerate() {
            stream.event(&json!({"type":"response.function_call_arguments.delta","sequence_number":index+1,
                "item_id":item["id"],"output_index":0,"delta":part})).unwrap();
        }
        stream.event(&json!({"type":"response.function_call_arguments.done","sequence_number":3,
            "item_id":item["id"],"output_index":0,"name":"ask","arguments":arguments})).unwrap();
        stream.event(&json!({"type":"response.output_item.done","sequence_number":4,
            "output_index":0,"item":item})).unwrap();
        stream.event(&json!({"type":"response.completed","sequence_number":5,
            "response":response_body(json!([item]))})).unwrap();
    }
    stream.finish();
}

#[test]
fn ask_tui_streaming_null_placeholder_waits_for_complete_arguments_and_response() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = ask_tui_start(&server, home.path(), project.path(), "response");
    pty.ready();
    pty.input("stream ask slowly\r");
    let (request, _) = pty.request_at(&server, "/v1/responses");
    let arguments = ask_single_arguments().to_string();
    let split = arguments.len() / 2;
    let call = ask_call_raw("response", "slow-ask", &arguments);
    let mut added = call.clone();
    added["status"] = json!("in_progress");
    added["arguments"] = Value::Null;
    let mut stream = request.start_sse().unwrap();
    stream.event(&json!({"type":"response.output_item.added", "sequence_number":0,
        "output_index":1, "item":added})).unwrap();
    stream.event(&json!({"type":"response.function_call_arguments.delta", "sequence_number":1,
        "item_id":call["id"], "output_index":1, "delta":&arguments[..split]})).unwrap();
    // 正文分片是接收栅栏，证明前面的起始项和半截参数已被消费；不靠 sleep 猜时序。
    stream.event(&json!({"type":"response.output_text.delta", "sequence_number":2,
        "item_id":"msg-stream", "output_index":0, "content_index":0,
        "delta":"ask-partial-received"})).unwrap();
    pty.text("ask-partial-received");
    assert!(!pty.parser.screen().contents().contains("╭ Ask"));
    assert!(!pty.parser.screen().contents().contains("invalid"));
    no_more(&server);

    stream.event(&json!({"type":"response.function_call_arguments.delta", "sequence_number":3,
        "item_id":call["id"], "output_index":1, "delta":&arguments[split..]})).unwrap();
    stream.event(&json!({"type":"response.function_call_arguments.done", "sequence_number":4,
        "item_id":call["id"], "output_index":1, "arguments":arguments})).unwrap();
    stream.event(&json!({"type":"response.output_item.done", "sequence_number":5,
        "output_index":1, "item":call})).unwrap();
    stream.event(&json!({"type":"response.output_text.delta", "sequence_number":6,
        "item_id":"msg-stream", "output_index":0, "content_index":0,
        "delta":" ask-json-received"})).unwrap();
    pty.text("ask-json-received");
    assert!(!pty.parser.screen().contents().contains("╭ Ask"),
        "参数完整后也须等整轮完成才执行 ask");
    no_more(&server);

    let message = response_message("msg-stream",
        json!([response_text("ask-partial-received ask-json-received")]));
    stream.event(&json!({"type":"response.completed", "sequence_number":7,
        "response":response_body(json!([message, call]))})).unwrap();
    stream.finish();
    ask_wait_panel(&mut pty, "Which database should the project use?");
    pty.input("\r");
    let (request, body) = pty.request_at(&server, "/v1/responses");
    let outputs = cancel_tool_outputs(&body, "response");
    assert_eq!(outputs, vec![("slow-ask".into(), json!({"ok":true,"answers":[{
        "id":"storage", "selected_options":["SQLite"], "custom_input":null
    }]}))]);
    ask_final_reply(request, "response", "slow ask done");
    pty.text("slow ask done");
    pty.ready();
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}

fn ask_final_reply(request: CapturedRequest, protocol: &str, text: &str) {
    if protocol == "completion" {
        final_reply(request, text);
    } else {
        response_final(request, text);
    }
}

fn ask_tui_start(server: &TestServer, home: &Path, project: &Path, protocol: &str) -> Pty {
    // 弹窗一秒淡出，避免与 5 秒栅栏竞争。
    crate::write_settings(home,
        r#"{"default_model":"fixture/primary","notification_timeout_seconds":1,"read":{"toolResultPreview":true}}"#,
    )
    .unwrap();
    std::fs::write(
        config_path(home, "models.yml"),
        response_models(server, Some(protocol)),
    )
    .unwrap();
    Pty::start_with_args(project, home, &[])
}

// 消息区没有自定义入口；圆角 Ask 标题和 Other 同时出现才代表面板打开。
fn ask_wait_panel(pty: &mut Pty, _question: &str) {
    pty.wait_for("ask panel opened", |screen| {
        let text = screen.contents();
        text.contains("Other (type your own)") && text.contains("╭ Ask")
    });
}

fn ask_wait_review(pty: &mut Pty) {
    pty.wait_for("ask review page", |screen| {
        let text = screen.contents();
        text.contains("Submit")
            && text.contains("Enter submit · Tab questions · Esc cancel")
    });
}

fn ask_wait_editor(pty: &mut Pty) {
    pty.wait_for("ask custom editor opened", |screen| {
        screen.contents().contains("Enter accept · Ctrl + J newline · Esc discard")
    });
}

// 面板位于 transcript 下方，从底部定位，避免误读 transcript 中重复的题目和选项。
fn ask_panel_find(screen: &vt100::Screen, needle: &str) -> (u16, u16) {
    let (rows, cols) = screen.size();
    for row in (0..rows).rev() {
        let line: String = (0..cols)
            .filter_map(|col| screen.cell(row, col))
            .map(|cell| {
                let text = cell.contents();
                if text.is_empty() { " " } else { text }
            })
            .collect();
        if let Some(index) = line.find(needle) {
            return (row, line[..index].chars().count() as u16);
        }
    }
    panic!("面板未显示 {needle}: {}", screen.contents());
}

fn ask_page_active(screen: &vt100::Screen, index: usize, total: usize) -> bool {
    screen.contents().contains(&format!("╭ Ask · {index}/{total} "))
}

fn ask_clear_draft(pty: &mut Pty) {
    pty.input("\x01\x0b");
    pty.draft("");
}

// 光标必须停在输入行草稿末尾：❯ 前缀占 2 列，排队草稿 四个宽字符占 8 列。
fn ask_assert_cursor_at_draft_end(pty: &Pty) {
    let screen = pty.parser.screen();
    let (row, column) = screen.cursor_position();
    let input_row = screen
        .contents()
        .lines()
        .position(|line| line.starts_with('❯'))
        .expect("input row with ❯ prompt");
    assert_eq!(usize::from(row), input_row, "{}", pty.diagnostics());
    assert_eq!(column, 10, "{}", pty.diagnostics());
}

// —— 验证项：多题、Review 修改、精确 answers（两协议）——

#[test]
fn ask_tui_single_space_selects_without_sending_until_enter() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = ask_tui_start(&server, home.path(), project.path(), protocol);
        pty.ready();
        pty.input("ask space task\r");
        let (request, _) = pty.request_at(&server, api_path(protocol));
        ask_reply_calls(
            request,
            protocol,
            json!([ask_call_json(protocol, "ask-space", ask_single_arguments())]),
        );
        ask_wait_panel(&mut pty, "Which database should the project use?");
        {
            let screen = pty.parser.screen();
            let (top, _) = ask_panel_find(screen, "╭ Ask");
            assert_eq!(ask_panel_find(screen, "Which database should the project use?").0, top + 1);
            assert_eq!(ask_panel_find(screen, "SQLite (Recommended)").0, top + 2);
            let text = screen.contents();
            assert!(!text.contains("Ask · running"), "等待时不应显示重复卡片：{text}");
            assert_eq!(text.matches("Which database should the project use?").count(), 1);
            assert_eq!(text.matches("Postgres").count(), 1);
            assert!(!screen.contents().contains("Tab/←/→ question"));
            assert!(!screen.contents().contains("Question 1"));
        }
        pty.input(" ");
        pty.wait_for("空格选中单选但不提交", |screen| {
            screen.contents().contains("󰐾 SQLite (Recommended)")
                && screen.contents().contains("Space select")
        });
        no_more(&server);
        pty.input("\x1b[B ");
        pty.wait_for("再次空格改选而不提交", |screen| {
            screen.contents().contains("󰐽 SQLite (Recommended)")
                && screen.contents().contains("󰐾 Postgres")
        });
        no_more(&server);
        pty.input("\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        pty.wait_for("回答后问答卡片进入消息区", |screen| {
            let text = screen.contents();
            text.contains("storage · Which database should the project use?")
                && text.contains("󰐾 Postgres") && !text.contains("╭ Ask")
        });
        assert_eq!(
            cancel_tool_outputs(&body, protocol),
            vec![("ask-space".into(), json!({"ok":true,"answers":[{
                "id":"storage", "selected_options":["Postgres"], "custom_input":null
            }]}))]
        );
        ask_final_reply(request, protocol, "space selection done");
        pty.text("space selection done");
        pty.input("\x03");
        pty.finish();
    }
}

#[test]
fn ask_tui_tabs_fit_current_page_and_keep_message_tail_above_panel() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = ask_tui_start(&server, home.path(), project.path(), protocol);
        pty.ready();
        pty.input("ask viewport task\r");
        let (request, _) = pty.request_at(&server, api_path(protocol));
        let args = json!({"questions":[
            {"id":"long", "header":"Scope", "question":"Choose the implementation scope",
             "options":[
                {"label":"Full", "description":"Include every supported operation"},
                {"label":"Core", "description":"Include the essential operations"},
                {"label":"Small", "description":"Include one operation"}
             ]},
            {"id":"short", "header":"Mode", "question":"Choose the execution mode",
             "options":[{"label":"Default", "preview":"ASK-MESSAGE-TAIL"}]}
        ]});
        let calls = json!([ask_call_json(protocol, "ask-viewport", args)]);
        if protocol == "completion" {
            reply(request, json!({"role":"assistant","content":"ASSISTANT-MESSAGE-TAIL","tool_calls":calls}));
        } else {
            let mut output = vec![response_message("msg-tail", json!([response_text("ASSISTANT-MESSAGE-TAIL")]))];
            output.extend(calls.as_array().unwrap().iter().cloned());
            response_reply(request, json!(output));
        }
        ask_wait_panel(&mut pty, "Choose the implementation scope");
        // 两题内容高度不同：面板取最高页的高度，切换页签不改外框高度与顶边。
        let mut wide = Vec::new();
        let mut narrow = Vec::new();
        for page in [1, 2] {
            for (rows, cols) in [(30, 100), (24, 60)] {
                pty.resize(rows, cols);
                pty.wait_for("消息尾部和当前页签可见", |screen| {
                    screen.size() == (rows, cols)
                        && ask_page_active(screen, page, 2)
                        && screen.contents().contains("ASSISTANT-MESSAGE-TAIL")
                });
                let screen = pty.parser.screen();
                let (top, _) = ask_panel_find(screen, "╭ Ask");
                let (bottom, _) = ask_panel_find(screen, "╰");
                assert_eq!(bottom - top + 1, 13, "面板高度应由最高页签决定");
                if cols == 100 {
                    wide.push(top);
                } else {
                    narrow.push(top);
                }
                let (tail, _) = ask_panel_find(screen, "ASSISTANT-MESSAGE-TAIL");
                let text = screen.contents();
                assert!(tail + 1 < top, "消息与面板之间应至少留出一行：{text}");
                assert!(
                    text.lines()
                        .nth(usize::from(top - 1))
                        .is_some_and(|row| row.trim().is_empty())
                );
                assert!(
                    text.lines()
                        .nth(usize::from(top + 1))
                        .is_some_and(|row| row.contains(" Scope   Mode   Review "))
                );
                assert!(!text.contains("ASK-MESSAGE-TAIL"), "未完成的 ask 详情不应进入消息区：{text}");
                assert!(!text.contains("Ask · running"));
            }
            pty.input("\t");
        }
        assert_eq!(wide[0], wide[1], "宽屏切换页签不应移动面板顶边");
        assert_eq!(narrow[0], narrow[1], "窄屏切换页签不应移动面板顶边");
        ask_wait_review(&mut pty);
        {
            let screen = pty.parser.screen();
            let (top, _) = ask_panel_find(screen, "╭ Ask");
            let (bottom, _) = ask_panel_find(screen, "╰");
            assert_eq!(bottom - top + 1, 13, "Review 页也应沿用最高页高度");
            assert_eq!(top, narrow[1], "切到 Review 不应移动面板顶边");
        }
        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn ask_tui_choice_descriptions_and_inline_editor_survive_resize() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = ask_tui_start(&server, home.path(), project.path(), "response");
    pty.ready();
    pty.input("ask layout task\r");
    let (request, _) = pty.request_at(&server, api_path("response"));
    let label = "PostgreSQL with connection pooling and managed backups";
    ask_reply_calls(request, "response", json!([ask_call_json("response", "ask-layout", json!({"questions":[{
        "id":"database", "header":"Database", "question":"Choose a database", "multi":true,
        "options":[
            {"label":label,"description":"Shared database for the production deployment."},
            {"label":"SQLite 本地数据库","description":"Embedded storage for local development."}
        ], "recommended":0
    }]}))]));
    ask_wait_panel(&mut pty, "Choose a database");
    {
        let screen = pty.parser.screen();
        let (row, _) = ask_panel_find(screen, label);
        let (description_row, _) = ask_panel_find(screen, "Shared database");
        assert_eq!(description_row, row + 1, "说明应紧接对应选项显示");
        let (title_row, _) = ask_panel_find(screen, "╭ Ask");
        assert_eq!(row, title_row + 3, "标题、页签和题干下方显示选项");
        assert_eq!(ask_panel_find(screen, " Database   Review ").0, title_row + 1);
        let (next_row, _) = ask_panel_find(screen, "SQLite");
        assert_eq!(next_row, row + 2, "下一选项应紧接说明，不插入空行");
        let panel_text = screen.contents().lines().skip(usize::from(title_row)).collect::<Vec<_>>().join("\n");
        assert!(panel_text.contains("Choose a database"));
        assert!(panel_text.contains("Shared database"));
        assert!(panel_text.contains("Embedded storage"));
        assert!(screen.contents().contains("󰄱 PostgreSQL"), "推荐不自动勾选");
    }
    pty.input(" \x1b[B");
    pty.wait_for("selection distinct from focus", |screen| {
        screen.contents().contains("󰄲 PostgreSQL")
            && screen.contents().contains("❯ 󰄱 SQLite")
    });
    pty.resize(30, 40);
    pty.wait_for("narrow choice layout", |screen| {
        screen.size().1 == 40
            && screen.contents().contains("󰄲 PostgreSQL")
            && screen.contents().contains("❯ 󰄱 SQLite")
    });
    {
        let screen = pty.parser.screen();
        let (row, _) = ask_panel_find(screen, "PostgreSQL");
        let (next_row, _) = ask_panel_find(screen, "SQLite");
        assert_eq!(next_row, row + 4, "两行名称与两行说明之后直接显示下一选项");
    }
    pty.input("\x1b[F");
    pty.wait_for("last choice remains reachable", |screen| {
        screen.contents().contains("❯ 󰄱 Other")
    });
    pty.resize(30, 100);
    pty.wait_for("返回宽屏后准备原位编辑", |screen| screen.size().1 == 100);
    let (title_before, _) = ask_panel_find(pty.parser.screen(), "╭ Ask");
    let (first_before, _) = ask_panel_find(pty.parser.screen(), label);
    let (other_before, _) = ask_panel_find(pty.parser.screen(), "Other (type your own)");
    let (bottom_before, _) = ask_panel_find(pty.parser.screen(), "╰");
    pty.input("custom layout answer");
    ask_wait_editor(&mut pty);
    pty.wait_for("自定义选项原位编辑且其他选项仍可见", |screen| {
        let text = screen.contents();
        text.contains("custom layout answer") && text.contains("󰄲 PostgreSQL")
            && text.contains("󰄱 SQLite") && !text.contains("custom answer · write")
    });
    {
        let screen = pty.parser.screen();
        let (row, column) = ask_panel_find(screen, "custom layout answer");
        assert_eq!(ask_panel_find(screen, "╭ Ask").0, title_before, "开始编辑不应移动外框");
        assert_eq!(ask_panel_find(screen, label).0, first_before, "开始编辑不应移动其他选项");
        assert_eq!(row, other_before, "编辑位置应保留在原选项行");
        assert_eq!(ask_panel_find(screen, "╰").0, bottom_before, "开始编辑不应改变外框高度");
        assert_eq!(screen.cursor_position(), (row, column + 20), "输入光标应位于选项原位");
    }
    pty.resize(30, 40);
    pty.wait_for("编辑中缩放仍保留其他选项和当前输入", |screen| {
        screen.size().1 == 40 && screen.contents().contains("custom layout answer")
            && screen.contents().contains("󰄲 PostgreSQL")
    });
    pty.resize(30, 100);
    pty.input("\r");
    ask_wait_review(&mut pty);
    pty.input("\r");
    let (request, body) = pty.request_at(&server, api_path("response"));
    let outputs = cancel_tool_outputs(&body, "response");
    assert!(outputs.iter().any(|(id, value)| id == "ask-layout"
        && value["answers"][0]["selected_options"] == json!([label])
        && value["answers"][0]["custom_input"] == "custom layout answer"));
    ask_final_reply(request, "response", "layout done");
    pty.text("layout done");
    pty.input("\x03");
    pty.finish();
}

#[test]
fn ask_tui_two_questions_review_modify_and_exact_answers() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = ask_tui_start(&server, home.path(), project.path(), protocol);
        pty.ready();
        pty.input("ask roundtrip task\r");
        let (request, _) = pty.request_at(&server, api_path(protocol));
        ask_reply_calls(
            request,
            protocol,
            json!([ask_call_json(protocol, "ask-review", ask_batch_arguments())]),
        );
        ask_wait_panel(&mut pty, "Which database should the project use?");
        // 推荐只是焦点：SQLite 带 (Recommended) 徽标但未被预选。
        {
            let text = pty.parser.screen().contents();
            assert!(
                text.contains("󰐽 SQLite (Recommended)") && text.contains("󰐽 Postgres"),
                "recommendation must be a display badge only\n{text}"
            );
            assert!(
                text.contains("╭ Ask · 1/2 "),
                "多题进度应显示在 Ask 标题中\n{text}"
            );
        }

        // Q1：焦点移到 Postgres 后 Enter 选中并前进。
        pty.input("\x1b[B\r");
        pty.wait_for("second question page", |screen| {
            ask_page_active(screen, 2, 2)
        });
        // Q2：Space 只勾选，Enter 只前进。
        pty.input(" ");
        pty.wait_for("unit checked", |screen| screen.contents().contains("󰄲 unit"));
        pty.input("\x1b[B ");
        pty.wait_for("lint checked", |screen| screen.contents().contains("󰄲 lint"));

        // 短屏 resize 不清选集：紧凑模式（去边框）仍显示选中摘要。
        pty.resize(8, 40);
        pty.wait_for("compact panel keeps summary", |screen| {
            screen.contents().contains("unit, lint")
        });
        pty.resize(30, 100);
        pty.wait_for("resize keeps selections", |screen| {
            screen.contents().contains("󰄲 lint")
        });
        pty.input("\r");
        ask_wait_review(&mut pty);
        {
            let text = pty.parser.screen().contents();
            assert!(
                text.contains("Postgres") && text.contains("unit, lint"),
                "review must summarize current answers\n{text}"
            );
        }

        // Review 回题修改：Q1 保持 Postgres 选中、SQLite 仍未选。
        pty.input("\t");
        pty.wait_for("back to first question", |screen| {
            ask_page_active(screen, 1, 2)
        });
        {
            let text = pty.parser.screen().contents();
            assert!(
                text.contains("󰐾 Postgres") && text.contains("󰐽 SQLite (Recommended)"),
                "selection and focus must survive page switches\n{text}"
            );
            let screen = pty.parser.screen();
            let (row, col) = ask_panel_find(screen, "󰐾 Postgres");
            assert_eq!(screen.cell(row, col).unwrap().fgcolor(), vt100::Color::Rgb(137, 210, 129));
        }
        pty.input("\t");
        pty.wait_for("back to second question", |screen| {
            ask_page_active(screen, 2, 2)
        });
        // 焦点仍在 lint：Space 先取消再重新勾选，提交前可以反复修改。
        pty.input(" ");
        pty.wait_for("lint unchecked", |screen| screen.contents().contains("󰄱 lint"));
        pty.input(" ");
        pty.wait_for("lint rechecked", |screen| screen.contents().contains("󰄲 lint"));
        pty.input("\r");
        ask_wait_review(&mut pty);
        pty.wait_for("review shows both checks again", |screen| {
            screen.contents().contains("unit, lint")
        });

        // Q2 添加自定义回答：Enter 打开编辑器，Ctrl-J 换行，Enter 接受非空白。
        pty.input("\t\t");
        pty.wait_for("back to second question again", |screen| {
            ask_page_active(screen, 2, 2)
        });
        pty.input("\x1b[B\x1b[B\r");
        ask_wait_editor(&mut pty);
        pty.input("再检查中文\x0a第二行");
        pty.input("\r");
        pty.wait_for("review shows custom answer", |screen| {
            screen.contents().contains("custom: 再检查中文")
        });
        pty.input("\r");

        // 下一轮请求包含原 call id 的精确 answers：问题原序、label 原文
        // （不带 Recommended 徽标）、多选按原选项顺序、custom 保留换行。
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_tool_outputs(&body, protocol),
            vec![(
                "ask-review".to_owned(),
                json!({"ok":true,"answers":[
                    {"id":"storage","selected_options":["Postgres"],"custom_input":null},
                    {"id":"checks","selected_options":["unit","lint"],"custom_input":"再检查中文\n第二行"}
                ]})
            )],
            "protocol {protocol}"
        );
        ask_final_reply(request, protocol, "ask roundtrip done");
        pty.wait_for("answer card records the submitted answers", |screen| {
            let text = screen.contents();
            text.contains("ask roundtrip done")
                && text.contains("Storage · Which database should the project use?")
                && text.contains("󰐾 Postgres")
                && text.contains("󰄲 unit")
                && text.contains("󰄲 lint")
                && text.contains("Other: 再检查中文")
                && !text.contains("Other (type your own)")
        });
        pty.ready();
        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}

// —— 验证项：空 options 自由回答 + 空多选显式提交 ——

#[test]
fn ask_tui_freeform_empty_options_and_empty_multi_submit() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = ask_tui_start(&server, home.path(), project.path(), protocol);
        pty.ready();

        // 空 options：只有自由回答入口，焦点直接落在 Other 上。
        pty.input("ask freeform task\r");
        let (request, _) = pty.request_at(&server, api_path(protocol));
        ask_reply_calls(
            request,
            protocol,
            json!([ask_call_json(
                protocol,
                "ask-free",
                json!({"questions":[{"id":"free","question":"Anything else?","options":[]}]})
            )]),
        );
        ask_wait_panel(&mut pty, "Anything else?");
        pty.input("\r");
        ask_wait_editor(&mut pty);
        pty.input("自定义回答内容");
        pty.input("\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_tool_outputs(&body, protocol),
            vec![(
                "ask-free".to_owned(),
                json!({"ok":true,"answers":[
                    {"id":"free","selected_options":[],"custom_input":"自定义回答内容"}
                ]})
            )],
            "protocol {protocol}"
        );
        ask_final_reply(request, protocol, "freeform done");
        pty.text("freeform done");
        pty.ready();

        // 多选可以显式提交空集：Review 显示 No options selected。
        pty.input("ask empty multi task\r");
        let (request, _) = pty.request_at(&server, api_path(protocol));
        ask_reply_calls(
            request,
            protocol,
            json!([ask_call_json(
                protocol,
                "ask-empty-multi",
                json!({"questions":[{"id":"tags","question":"Pick tags?","multi":true,
                    "options":[{"label":"alpha"},{"label":"beta"}]}]})
            )]),
        );
        ask_wait_panel(&mut pty, "Pick tags?");
        pty.input("\r");
        ask_wait_review(&mut pty);
        pty.wait_for("empty multi summary", |screen| {
            screen.contents().contains("No options selected")
        });
        pty.input("\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_tool_outputs(&body, protocol),
            vec![
                ("ask-free".to_owned(), json!({"ok":true,"answers":[
                    {"id":"free","selected_options":[],"custom_input":"自定义回答内容"}
                ]})),
                ("ask-empty-multi".to_owned(), json!({"ok":true,"answers":[
                    {"id":"tags","selected_options":[],"custom_input":null}
                ]})),
            ],
            "protocol {protocol}"
        );
        ask_final_reply(request, protocol, "empty multi done");
        pty.text("empty multi done");
        pty.ready();
        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}

// —— 验证项：畸形参数不开面板、不挂起、正常工具错误 ——

include!("ask_tui/lifecycle_cases.rs");
