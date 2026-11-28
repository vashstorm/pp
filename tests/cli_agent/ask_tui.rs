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

// 摘要不影响模型正文；恢复、预览配置和原生历史均不显示 read 正文。
#[test]
fn tool_folding_omp_read_default_summary_resume_and_native_scrollback() {
    let protocol = "response";
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
        text.contains("Other (type your own)") && text.contains("╭ \u{f02d7} Ask")
    });
}

fn ask_wait_review(pty: &mut Pty) {
    pty.wait_for("ask review page", |screen| {
        let text = screen.contents();
        text.contains("╭ \u{f02d7} Ask · Submit")
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
    screen.contents().contains(&format!("╭ \u{f02d7} Ask · {index}/{total} "))
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
fn ask_tui_two_questions_review_modify_and_exact_answers() {
    let protocol = "response";
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
                ask_page_active(pty.parser.screen(), 1, 2),
                "Ask 标题应显示图标与多题进度\n{text}"
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

// —— 验证项：空 options 自由回答 + 空多选显式提交 ——

// —— 验证项：畸形参数不开面板、不挂起、正常工具错误 ——

include!("ask_tui/lifecycle_cases.rs");
