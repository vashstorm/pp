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
        pty.wait_for("read 三行预览和 bash 十行尾部", |screen| {
            let text = screen.contents();
            text.matches("Ctrl + O to expand").count() == 3
                && text.contains("fold-first-02") && text.contains("fold-second-02")
                && text.contains("fold-bash-10") && !text.contains("fold-bash-00")
                && !text.contains("fold-first-19") && !text.contains("fold-second-19")
        });
        pty.input("\x1b[B ");
        pty.wait_for("选择答案后保持面板", |screen| screen.contents().contains("󰐾 Postgres"));
        pty.input("\x0f");
        pty.wait_for("ask 焦点下全局展开", |screen| {
            let text = screen.contents();
            text.contains("fold-first-19") && text.contains("fold-second-19")
                && text.contains("fold-bash-00") && !text.contains("to expand")
                && text.contains("󰐾 Postgres") && text.contains("保留草稿")
        });
        pty.input("\x0f");
        pty.wait_for("ask 焦点下全局收起", |screen| {
            let text = screen.contents();
            text.matches("Ctrl + O to expand").count() == 3
                && !text.contains("fold-first-19") && !text.contains("fold-second-19")
                && !text.contains("fold-bash-00") && text.contains("󰐾 Postgres")
        });
        pty.input("\x0f");
        pty.text("fold-first-19");
        pty.input("\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(outputs.len(), 4);
        assert!(outputs[0].1["content"].as_str().unwrap().contains("fold-first-19"));
        assert!(outputs[1].1["content"].as_str().unwrap().contains("fold-second-19"));
        assert!(outputs[2].1["stdout"].as_str().unwrap().contains("fold-bash-00"));
        assert_eq!(outputs[3].1["answers"][0]["selected_options"], json!(["Postgres"]));
        ask_reply_calls(request, protocol, json!([
            mk_call("third", "read", json!({"path":"third"}))
        ]));
        let (request, _) = pty.request_at(&server, api_path(protocol));
        pty.wait_for("后续工具继承展开状态", |screen| {
            let text = screen.contents();
            text.contains("fold-third-19") && !text.contains("to expand")
        });
        ask_final_reply(request, protocol, "global folding done");
        pty.text("global folding done");
        pty.ready();
        pty.draft("保留草稿");
        ask_assert_cursor_at_draft_end(&pty);
        pty.input("\x0f");
        pty.wait_for("任务结束后仍可全局折叠", |screen| {
            screen.contents().matches("Ctrl + O to expand").count() == 4
        });
        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}

// 默认摘要不影响模型正文；恢复采用当前配置，原生历史保留提交时的摘要。
#[test]
fn tool_folding_omp_read_default_summary_resume_and_native_scrollback() {
    for protocol in ["completion", "response"] {
        let provider = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), r#"{"notification_timeout_seconds":1}"#).unwrap();
        std::fs::write(config_path(home.path(), "models.yml"), response_models(&provider, Some(protocol))).unwrap();
        for name in ["one", "two"] {
            std::fs::write(project.path().join(name), (0..20).map(|i|format!("body-{name}-{i:02}\n")).collect::<String>()).unwrap();
        }
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
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
        pty.input("\x0fbarrier");
        pty.draft("barrier");
        assert!(!pty.parser.screen().contents().contains("body-one-"));
        ask_final_reply(request, protocol, "summary done");
        pty.text("summary done");
        pty.ready();
        let (scrollback, _) = diff_blocks_exit_scrollback(&mut pty, "summary done");
        assert!(scrollback.contains("Read (2)"));
        assert!(!scrollback.contains("body-one-19") && !scrollback.contains("body-two-19"));
        pty.finish();
        let id = saved_session_id(home.path());
        std::fs::write(config_path(home.path(), "settings.yml"), r#"{"notification_timeout_seconds":1,"read":{"toolResultPreview":true}}"#).unwrap();
        let mut resumed = Pty::start_with_args(project.path(), home.path(), &["-r", &id]);
        resumed.resize(80, 100);
        resumed.ready();
        resumed.wait_for("恢复后采用当前 read 预览配置", |screen| {
            let text = screen.contents();
            text.contains("body-one-02") && !text.contains("body-one-03") && text.contains("Ctrl + O to expand")
        });
        resumed.input("\x0f");
        resumed.text("body-one-19");
        resumed.input("\x03");
        resumed.finish();
        no_more(&provider);
    }
}

#[test]
fn tool_folding_omp_native_search_clips_long_rows_for_both_protocols() {
    for protocol in ["completion", "response"] {
        for name in ["grep", "find"] {
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
            let schema = body["tools"].as_array().unwrap().iter().map(|t| if protocol == "completion" { &t["function"] } else { t })
                .find(|t| t["name"] == name).unwrap();
            let args = if name == "grep" { json!({"args":["-n","-e","needle","lines"]}) }
                else if schema["description"].as_str().unwrap().starts_with("Uses fd.") { json!({"args":[".","files"]}) }
                else { json!({"args":["files","-type","f"]}) };
            ask_reply_calls(request, protocol, json!([diff_blocks_call(protocol,"search",name,args)]));
            let (request, body) = pty.request_at(&provider, api_path(protocol));
            let outputs = cancel_tool_outputs(&body, protocol);
            assert_eq!(outputs[0].1["ok"], true);
            assert_eq!(outputs[0].1["stdout"].as_str().unwrap().lines().count(),20);
            let marker = if name == "grep" { "entry-" } else { "path-" };
            let shown = if name == "grep" { 5 } else { 8 };
            pty.wait_for("搜索正文按额度及列宽收敛", |screen| {
                let text = screen.contents();
                text.matches(marker).count() == shown && text.contains("more") && text.contains("Ctrl + O")
            });
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
        settings["mcp_servers"]["local"]["env"]["MCP_VALUE"] = json!(&encoded[1..encoded.len()-1]);
        std::fs::write(config_path(home.path(), "settings.yml"), settings.to_string()).unwrap();
        std::fs::write(config_path(home.path(), "models.yml"), response_models(&provider, Some(protocol))).unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
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
        pty.wait_for("MCP 四行与 write 六行预览", |screen| {
            let text = screen.contents();
            text.contains("mcp-body-03") && !text.contains("mcp-body-04")
                && text.contains("write-body-05") && !text.contains("write-body-06")
        });
        pty.input("\x0fa"); pty.draft("a");
        let expanded = pty.parser.screen().contents();
        assert!(expanded.contains("mcp-body-11") && !expanded.contains("mcp-body-12"));
        assert!(expanded.contains("write-body-19"));
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

fn ask_reply_calls(request: Request, protocol: &str, calls: Value) {
    if protocol == "completion" {
        calls_reply(request, calls);
    } else {
        response_reply(request, calls);
    }
}

// 覆盖完整参数快照与分片 SSE；Responses 起始项省略 arguments 的格式也必须可用。
fn ask_provider_reply(request: Request, protocol: &str, id: &str, arguments: &str, fragmented: bool) {
    if !fragmented {
        ask_reply_calls(request, protocol, json!([ask_call_raw(protocol, id, arguments)]));
        return;
    }
    let split = arguments.char_indices().nth(arguments.chars().count() / 2).unwrap().0;
    let parts = [&arguments[..split], &arguments[split..]];
    let mut stream = SseReply::new(request);
    if protocol == "completion" {
        stream.event(chat_chunk(json!({"role":"assistant","tool_calls":[{
            "index":0,"id":id,"type":"function","function":{"name":"ask","arguments":parts[0]}
        }]}), None));
        stream.event(chat_chunk(json!({"tool_calls":[{
            "index":0,"function":{"arguments":parts[1]}
        }]}), None));
        stream.event(chat_chunk(json!({}), Some("tool_calls")));
        stream.frame("[DONE]");
    } else {
        let item = ask_call_raw(protocol, id, arguments);
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        added.as_object_mut().unwrap().remove("arguments");
        stream.event(json!({"type":"response.output_item.added","sequence_number":0,
            "output_index":0,"item":added}));
        for (index, part) in parts.into_iter().enumerate() {
            stream.event(json!({"type":"response.function_call_arguments.delta","sequence_number":index+1,
                "item_id":item["id"],"output_index":0,"delta":part}));
        }
        stream.event(json!({"type":"response.function_call_arguments.done","sequence_number":3,
            "item_id":item["id"],"output_index":0,"name":"ask","arguments":arguments}));
        stream.event(json!({"type":"response.output_item.done","sequence_number":4,
            "output_index":0,"item":item}));
        stream.event(json!({"type":"response.completed","sequence_number":5,
            "response":response_body(json!([item]))}));
    }
    stream.finish();
}

fn ask_final_reply(request: Request, protocol: &str, text: &str) {
    if protocol == "completion" {
        final_reply(request, text);
    } else {
        response_final(request, text);
    }
}

fn ask_tui_start(server: &Server, home: &Path, project: &Path, protocol: &str) -> Pty {
    // 弹窗一秒淡出，避免与 5 秒栅栏竞争。
    std::fs::write(
        config_path(home, "settings.yml"),
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
        assert_eq!(row, title_row + 1, "标题下方直接显示选项");
        let (next_row, _) = ask_panel_find(screen, "SQLite");
        assert_eq!(next_row, row + 2, "下一选项应紧接说明，不插入空行");
        let panel_text = screen.contents().lines().skip(usize::from(title_row)).collect::<Vec<_>>().join("\n");
        assert!(!panel_text.contains("Choose a database"));
        assert!(!panel_text.contains("Database"), "面板不应重复显示题目标签");
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

#[test]
fn ask_tui_provider_argument_variants_succeed_without_model_retry() {
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
fn ask_tui_esc_cancel_keeps_draft_skips_batch_write_and_pairs_history() {
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
    std::fs::write(config_path(home.path(), "settings.yml"), fixture_settings()).unwrap();
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
    pty.ready_model("vendor-b");
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
        screen.contents().contains("Resumed session:")
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
    resumed.wait_for("error popup fades", |screen| {
        notification_bounds(screen, "Error", vt100::Color::Rgb(224, 108, 117)).is_none()
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
