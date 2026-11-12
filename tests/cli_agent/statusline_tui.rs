// 标题专用测试直接控制旁路请求，不使用通用夹具的自动标题响应。
fn statusline_home(server: &Server, protocol: &str, fast: bool) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let other = if protocol == "completion" {
        "response"
    } else {
        "completion"
    };
    let role = if fast {
        "  fast: {model: title/fast, effort: none}\n"
    } else {
        ""
    };
    std::fs::write(config_path(home.path(), "settings.yml"), format!("default_model: main/primary\nrate_limit_retry_budget_seconds: 0\nauto_compact: false\nmodel_roles:\n  slow: {{model: main/primary, effort: none}}\n{role}")).unwrap();
    let model = |name: &str, rate: f64| json!({"model":name,"context_window":40000,"cost":{"input":rate,"cacheRead":rate,"cacheWrite":rate,"output":rate}});
    std::fs::write(config_path(home.path(), "models.yml"), json!({"providers":{
        "main":slash_provider(server,"main",protocol,json!({"primary":model("main-model",1.0)})),
        "title":slash_provider(server,"title",other,json!({"fast":model("title-model",2.0)}))
    }}).to_string()).unwrap();
    home
}

fn statusline_start(server: &Server, protocol: &str, fast: bool) -> (tempfile::TempDir, Pty) {
    let home = statusline_home(server, protocol, fast);
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready();
    pty.resize(30, 180);
    (home, pty)
}

fn statusline_requests(server: &Server) -> ((Request, Value), (Request, Value)) {
    let first = receive_raw(server);
    let second = receive_raw(server);
    if is_task_title(&first.1) {
        assert!(!is_task_title(&second.1));
        (second, first)
    } else {
        assert!(is_task_title(&second.1));
        (first, second)
    }
}

fn statusline_reply_usage(request: Request, protocol: &str, text: &str) {
    let mut stream = SseReply::new(request);
    streaming_delta(&mut stream, protocol, text);
    context_tui_stream_usage(&mut stream, protocol, text);
    stream.finish();
}

fn statusline_reply_calls_usage(request: Request, protocol: &str, mut calls: Value) {
    let mut stream = SseReply::new(request);
    if protocol == "completion" {
        for (index, call) in calls.as_array_mut().unwrap().iter_mut().enumerate() {
            call["index"] = json!(index);
        }
        stream.event(chat_chunk(
            json!({"role":"assistant","tool_calls":calls}),
            None,
        ));
        stream.event(chat_chunk(json!({}), Some("tool_calls")));
        stream.event(json!({"id":"chatcmpl-cost", "object":"chat.completion.chunk", "created":1,
            "model":"main-model", "choices":[], "usage":{"prompt_tokens":12000,"completion_tokens":300,
            "total_tokens":12300,"prompt_tokens_details":{"cached_tokens":40,"cache_write_tokens":120}}}));
        stream.frame("[DONE]");
    } else {
        let mut response = response_body(calls);
        response["usage"] = json!({"input_tokens":12000,"output_tokens":300,"total_tokens":12300,
            "input_tokens_details":{"cached_tokens":40,"cache_write_tokens":120},"output_tokens_details":{"reasoning_tokens":0}});
        stream
            .event(json!({"type":"response.completed","sequence_number":100,"response":response}));
    }
    stream.finish();
}

#[test]
fn slow_statusline_out_accumulates_session_usage_across_requests_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = statusline_start(&fixture, protocol, false);
        pty.input("live out task\r");
        let ((main, _), (title, _)) = statusline_requests(&fixture);
        statusline_reply_usage(title, protocol, "实时输出");
        let mut stream = SseReply::new(main);
        // 19 字节正文；本次请求尚未收尾时，out 只能来自字节估算。
        streaming_delta(&mut stream, protocol, "statusline-live-out");
        pty.text("statusline-live-out");
        pty.wait_for("生成中的 out 按已接收字节实时增长", |screen| {
            screen.contents().contains("\u{f04e2} —/7")
        });
        context_tui_stream_usage(&mut stream, protocol, "statusline-live-out");
        stream.finish();
        pty.completed_after("statusline-live-out");
        pty.wait_for("首个请求完成后 out 等于会话累计", |screen| {
            screen.contents().contains("\u{f04e2} 12K/300")
        });
        // 同一会话的第二个请求：out 在上一笔 300 上叠加，而不是重新从本次 usage 开始。
        pty.input("second out task\r");
        let ((main, _), (title, _)) = statusline_requests(&fixture);
        statusline_reply_usage(title, protocol, "第二任务输出");
        statusline_reply_usage(main, protocol, "statusline-second-out");
        pty.completed_after("statusline-second-out");
        pty.wait_for("第二次请求后 out 累计为整个会话", |screen| {
            screen.contents().contains("\u{f04e2} 12K/600")
        });
        pty.input("/new\r");
        pty.wait_for("新会话清空累计输出", |screen| {
            screen.contents().contains("\u{f04e2} —/—")
        });
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn statusline_cost_displays_only_settled_session_subtotal() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = statusline_start(&fixture, protocol, false);
        pty.input("live cost task\r");
        let ((main, _), (title, _)) = statusline_requests(&fixture);
        // 标题请求保持未应答，两个请求都还没有结算。
        let mut stream = SseReply::new(main);
        streaming_delta(&mut stream, protocol, "statusline-live-cost");
        pty.text("statusline-live-cost");
        pty.wait_for("首笔费用结算前隐藏费用字段", |screen| {
            let text = screen.contents();
            text.contains("\u{f04e2} —/7") && !text.contains("\u{f01c1}")
        });
        // 标题在任务结束前落定，否则会被取消而变成未知费用。
        statusline_reply_usage(title, protocol, "实时费用标题");
        pty.text("实时费用标题");
        pty.wait_for("主请求生成期间仅显示已结算的标题费用", |screen| {
            let text = screen.contents();
            text.contains("\u{f01c1} 0.012 ·") && !text.contains("(~")
        });
        context_tui_stream_usage(&mut stream, protocol, "statusline-live-cost");
        stream.finish();
        pty.completed_after("statusline-live-cost");
        pty.wait_for("两笔结算后显示累计金额", |screen| {
            screen.contents().contains("\u{f01c1} 0.025 ·")
        });
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn slow_statusline_subagent_badge_tracks_running_children_before_speed() {
    let fixture = server();
    let protocol = "completion";
    let (_home, mut pty) = statusline_start(&fixture, protocol, false);
    pty.input("track four subagents\r");
    let ((root, _), (title, _)) = statusline_requests(&fixture);
    statusline_reply_usage(title, protocol, "子代理状态");
    statusline_reply_calls_usage(
        root,
        protocol,
        json!([
            hub_call(
                protocol,
                "spawn-badge",
                "task",
                json!({"context":"status badge", "tasks": [
                    {"role":"slow","task":"badge first"}, {"role":"slow","task":"badge second"},
                    {"role":"slow","task":"badge third"}, {"role":"slow","task":"badge fourth"}
                ]})
            ),
            hub_call(protocol, "wait-badge", "wait", json!({}))
        ]),
    );
    // 保持子请求未应答，逐个完成，以屏幕内容作为状态切换的同步点。
    let mut children: Vec<_> = (0..4)
        .map(|_| {
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            assert!(hub_is_child(&body), "{body}");
            request
        })
        .collect();
    for (count, icon) in [
        (4, '\u{f0c0}'),
        (3, '\u{f0c0}'),
        (2, '\u{f000e}'),
        (1, '\u{f007}'),
    ] {
        let badge = format!("{icon} {count} · ");
        pty.wait_for(
            &format!("{count} 个运行中的子代理显示在速度前"),
            |screen| {
                screen.contents().lines().any(|line| {
                    line.split_once(&badge).is_some_and(|(_, after)| {
                        after.starts_with(['\u{f0f86}', '\u{f0f85}', '\u{f04c5}'])
                            && after.contains("tok/s")
                    })
                })
            },
        );
        let screen = pty.parser.screen();
        let (row, col) = context_tui_locate(screen, &badge).unwrap();
        assert_eq!(
            screen.cell(row, col).unwrap().fgcolor(),
            vt100::Color::Rgb(254, 188, 56)
        );
        statusline_reply_usage(children.pop().unwrap(), protocol, "badge child done");
    }
    let (root, body) = pty.request_at(&fixture, api_path(protocol));
    assert!(!hub_is_child(&body), "{body}");
    pty.wait_for("子代理结束后只保留速度", |screen| {
        let text = screen.contents();
        text.contains("tok/s") && !text.contains(['\u{f007}', '\u{f000e}', '\u{f0c0}'])
    });
    statusline_reply_usage(root, protocol, "statusline-badge-done");
    pty.completed_after("statusline-badge-done");
    pty.input("\x03");
    pty.finish();
    no_more(&fixture);
}

#[test]
fn task_status_cost_includes_child_subagents_without_double_counting_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = statusline_start(&fixture, protocol, false);
        pty.input("root task with sibling children\r");
        let ((root, _), (title, _)) = statusline_requests(&fixture);
        statusline_reply_usage(title, protocol, "同级任务计费");
        pty.text("同级任务计费");
        // 一批派发两个同级 child 后在同批内等待，父方不产生额外的中间请求。
        statusline_reply_calls_usage(
            root,
            protocol,
            json!([
                hub_call(protocol, "spawn-cost", "task", json!({"context":"sibling cost inheritance",
                    "tasks":[{"role":"slow","task":"first-cost-marker"},{"role":"slow","task":"second-cost-marker"}]})),
                hub_call(protocol, "wait-cost", "wait", json!({}))
            ]),
        );
        let mut children: Vec<Option<Request>> = vec![None, None];
        while children.iter().any(Option::is_none) {
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            assert!(
                hub_is_child(&body),
                "unexpected parent request before children finish: {body}"
            );
            let text = body.to_string();
            let index = ["first-cost-marker", "second-cost-marker"]
                .iter()
                .position(|marker| text.contains(*marker))
                .expect("known child task");
            assert!(children[index].is_none(), "duplicate child request: {body}");
            children[index] = Some(request);
        }
        statusline_reply_usage(children[0].take().unwrap(), protocol, "first-cost-answer");
        statusline_reply_usage(children[1].take().unwrap(), protocol, "second-cost-answer");
        let (root, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(!is_task_title(&body));
        assert!(!hub_is_child(&body), "{body}");
        let text = body.to_string();
        assert!(text.contains("first-cost-answer"));
        assert!(text.contains("second-cost-answer"));
        statusline_reply_usage(root, protocol, "root-cost-answer");
        pty.completed_after("root-cost-answer");
        // 标题 + 根两次 + 两个同级 child 各一次，共五次模型请求。
        pty.wait_for("根任务累计子任务费用", |screen| {
            screen.contents().contains("\u{f01c1} 0.061 ·")
        });
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn task_status_title_parallel_fast_both_protocols_cost_and_history_isolation() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (home, mut pty) = statusline_start(&fixture, protocol, true);
        pty.input("修复会话历史 sqlite\r");
        // 两个请求都在任何响应前到达，证明标题不会阻塞主请求。
        let ((main, body), (title, title_body)) = statusline_requests(&fixture);
        let title_protocol = if protocol == "completion" {
            "response"
        } else {
            "completion"
        };
        assert_eq!(body["model"], "main-model");
        assert_eq!(title_body["model"], "title-model");
        assert!(title_body["tools"].is_null());
        assert!(title_body["tool_choice"].is_null());
        assert_eq!(title.url(), api_path(title_protocol));
        assert!(!body.to_string().contains("Generate a short task title"));
        statusline_reply_usage(title, title_protocol, "修复会话历史");
        pty.wait_for("后台标题和费用先于主回复到达", |screen| {
            let text = screen.contents();
            text.contains("修复会话历史")
                && text.contains("0.025")
                && text.contains("\u{f04e2} —/—")
        });
        assert!(
            context_tui_speed(pty.parser.screen()).is_none(),
            "标题不能污染主请求速度"
        );
        statusline_reply_usage(main, protocol, "statusline-main-answer");
        pty.completed_after("statusline-main-answer");
        pty.wait_for("主请求与标题费用只累计一次", |screen| {
            screen.contents().contains("0.037")
        });
        let screen = pty.parser.screen();
        let (row, col) = context_tui_locate(screen, "0.037").unwrap();
        assert_eq!(
            screen.cell(row, col).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
        let (title_row, title_col) = context_tui_locate(screen, "\u{f09a9} 修复会话历史").unwrap();
        assert_eq!(title_row, row);
        assert_eq!(
            screen.cell(row, title_col).unwrap().fgcolor(),
            vt100::Color::Rgb(254, 188, 56)
        );
        let sessions = home.path().join(".pp/agent/sessions");
        let journal = std::fs::read_dir(sessions)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
            .unwrap();
        let history = std::fs::read_to_string(journal.path()).unwrap();
        assert!(!history.contains("Generate a short task title"));
        assert!(!history.contains("task_title"));
        let db = rusqlite::Connection::open(home.path().join(".pp/stats/stats.db")).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM requests WHERE purpose='task_title'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM request_contexts",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        pty.input("/new\r");
        pty.wait_for("新会话清空标题、费用和累计输出", |screen| {
            let text = screen.contents();
            text.contains("0.0%/40K")
                && text.contains("\u{f04e2} —/—")
                && !text.contains("\u{f09a9}")
                && !text.contains("\u{f01c1}")
                && !text.contains("\u{f051b}")
        });
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn task_status_title_without_fast_cancel_and_next_generation_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = statusline_start(&fixture, protocol, false);
        pty.input("first task\r");
        let ((main, _), (old_title, title_body)) = statusline_requests(&fixture);
        assert_eq!(title_body["model"], "main-model");
        let mut stream = SseReply::new(main);
        streaming_delta(&mut stream, protocol, "first-task-partial");
        pty.text("first-task-partial");
        pty.input("\x1b");
        // 标题仍被服务端握住时，取消必须完成并恢复输入。
        cancel_wait_cancelled_ready(&mut pty);
        pty.input("second task\r");
        let ((main, body), (title, _)) = statusline_requests(&fixture);
        assert!(!body.to_string().contains("Generate a short task title"));
        let _ = old_title.respond(Response::empty(204));
        drop(stream);
        statusline_reply_usage(title, protocol, "第二任务标题");
        pty.text("第二任务标题");
        statusline_reply_usage(main, protocol, "second-task-answer");
        pty.completed_after("second-task-answer");
        pty.wait_for("请求取消后显示已知费用小计", |screen| {
            screen.contents().contains("\u{f01c1} 0.025 ·")
        });
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn task_status_title_failure_is_hidden_and_displays_known_cost() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = statusline_start(&fixture, protocol, false);
        pty.input("title failure task\r");
        let ((main, _), (title, _)) = statusline_requests(&fixture);
        title
            .respond(Response::from_string("{}").with_status_code(500))
            .unwrap();
        statusline_reply_usage(main, protocol, "main-still-completes");
        pty.completed_after("main-still-completes");
        pty.wait_for("标题请求失败后只显示主请求费用", |screen| {
            screen.contents().contains("\u{f01c1} 0.012 ·")
        });
        assert!(!pty.parser.screen().contents().contains("\u{f09a9}"));
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn task_status_title_fallback_cross_protocol_retains_no_tools_and_actual_price() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = statusline_home(&fixture, protocol, true);
        std::fs::write(config_path(home.path(), "settings.yml"), "default_model: main/primary\nrate_limit_retry_budget_seconds: 0\nauto_compact: false\nmodel_roles:\n  fast: {model: title/fast, effort: none, fallback: {model: main/primary, effort: none}}\n").unwrap();
        let mut pty = Pty::start_with_existing_files(home.path());
        pty.ready();
        pty.resize(30, 180);
        pty.input("fallback title task\r");
        let ((main, _), (title, _)) = statusline_requests(&fixture);
        title
            .respond(Response::from_string("{}").with_status_code(500))
            .unwrap();
        let (title, body) = receive_raw(&fixture);
        assert!(is_task_title(&body));
        assert_eq!(title.url(), api_path(protocol));
        assert_eq!(body["model"], "main-model");
        assert!(body["tools"].is_null() && body["tool_choice"].is_null());
        statusline_reply_usage(title, protocol, "备用标题");
        pty.text("备用标题");
        statusline_reply_usage(main, protocol, "fallback-main-answer");
        pty.completed_after("fallback-main-answer");
        pty.wait_for("备用标题按实际模型计价", |screen| {
            screen.contents().contains("\u{f01c1} 0.025 ·")
        });
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn task_status_cost_survives_statistics_write_failure_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (home, mut pty) = statusline_start(&fixture, protocol, false);
        pty.input("statistics unavailable task\r");
        let ((main, _), (title, _)) = statusline_requests(&fixture);
        statusline_reply_usage(title, protocol, "独立费用快照");
        pty.wait_for("标题请求完成并保存统计", |screen| {
            screen.contents().contains("0.012")
        });
        // 金额先于统计落盘更新，等待标题本身发布作为写入完成栅栏。
        pty.text("独立费用快照");
        let db = rusqlite::Connection::open(home.path().join(".pp/stats/stats.db")).unwrap();
        // 删除写路径依赖的表，让下一次统计写入失败但不影响费用累计。
        db.execute_batch("DROP TABLE request_tools;").unwrap();
        statusline_reply_usage(main, protocol, "statistics-failure-main-answer");
        pty.completed_after("statistics-failure-main-answer");
        pty.wait_for("统计写失败不影响费用累计", |screen| {
            screen.contents().contains("\u{f01c1} 0.025 ·")
        });
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn statusline_session_cost_resumes_immediately_and_reuses_web_pricing() {
    let fixture = server();
    let protocol = "completion";
    let home = statusline_home(&fixture, protocol, false);
    let settings = config_path(home.path(), "settings.yml");
    let mut text = std::fs::read_to_string(&settings).unwrap();
    text.push_str("cost: {currency: CNY, exchange_rate: 7.2}\n");
    std::fs::write(settings, text).unwrap();
    let models = config_path(home.path(), "models.yml");
    let mut catalog: Value = serde_json::from_slice(&std::fs::read(&models).unwrap()).unwrap();
    catalog["providers"]["main"]["models"]["primary"] =
        json!({"model":"gateway/OpenAI/GPT-6.1-SOL","context_window":40000});
    std::fs::write(models, catalog.to_string()).unwrap();

    let mut pty = Pty::start_with_existing_files(home.path());
    pty.resize(30, 180);
    pty.ready();
    for (task, title_text, answer, total) in [
        ("first session cost task", "第一笔费用", "cost-answer-one", "0.389"),
        ("second session cost task", "第二笔费用", "cost-answer-two", "0.777"),
    ] {
        pty.input(&format!("{task}\r"));
        let ((main, body), (title, _)) = statusline_requests(&fixture);
        assert_eq!(body["model"], "gateway/OpenAI/GPT-6.1-SOL");
        statusline_reply_usage(title, protocol, title_text);
        pty.text(title_text);
        statusline_reply_usage(main, protocol, answer);
        pty.completed_after(answer);
        pty.text(&format!("\u{f07ba} {total}"));
    }
    let id = saved_session_id(home.path());
    pty.input("/new\r");
    pty.wait_for("新会话清空费用", |screen| {
        let text = screen.contents();
        text.contains("0.0%/40K") && !text.contains('\u{f07ba}')
    });
    pty.input("/resume\r");
    pty.wait_for("列出待恢复会话", |screen| {
        let text = screen.contents();
        text.contains("Resume session")
            && text.contains("first session cost task")
            && text.lines().any(|line| line.starts_with('>'))
    });
    pty.input("\r");
    pty.wait_for("恢复后无需新请求即可看到历史费用", |screen| {
        let text = screen.contents();
        text.contains("cost-answer-two")
            && text.contains("\u{f07ba} 0.777")
            && !text.contains("0.777 CNY")
    });
    pty.input("/quit\r");
    pty.finish();
    no_more(&fixture);

    // 首个标题也必须已关联；Web 的 USD 原值与状态栏换算使用同一费率。
    let rows = super::stats::rows(home.path());
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().all(|row| row["session_file"]
        .as_str()
        .is_some_and(|path| path.ends_with(&format!("{id}.jsonl")))));
    let (_stats, address) = super::stats::start_stats_server(home.path());
    let (status, body) = super::stats::http(&address, "/api/stats/overview?range=all", &address);
    assert_eq!(status, 200);
    let stats: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(stats["summary"]["priced"], 4);
    assert!((stats["summary"]["cost"].as_f64().unwrap() - 0.107936).abs() < 1e-12);

    let mut pty = Pty::start_with_args(home.path(), home.path(), &["-r", &id]);
    pty.resize(30, 180);
    pty.text("\u{f07ba} 0.777");
    tree_tui_open(&mut pty, "cost-answer-two");
    tree_tui_rewind(&mut pty, &[TREE_TUI_UP], "second session cost task");
    pty.text("\u{f07ba} 0.777");
    pty.input("continue after restoring cost\r");
    let ((main, _), (title, _)) = statusline_requests(&fixture);
    statusline_reply_usage(title, protocol, "恢复后的标题");
    pty.text("恢复后的标题");
    statusline_reply_usage(main, protocol, "cost-answer-three");
    pty.completed_after("cost-answer-three");
    pty.text("\u{f07ba} 1.166");
    pty.input("/quit\r");
    pty.finish();
    no_more(&fixture);
}
