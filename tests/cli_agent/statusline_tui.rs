// 标题专用测试直接控制旁路请求，不使用通用夹具的自动标题响应。
fn statusline_home(server: &TestServer, protocol: &str, fast: bool) -> tempfile::TempDir {
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

fn statusline_start(server: &TestServer, protocol: &str, fast: bool) -> (tempfile::TempDir, Pty) {
    let home = statusline_home(server, protocol, fast);
    let mut pty = Pty::start_with_args(home.path(), home.path(), &["--yolo"]);
    pty.ready();
    pty.resize(30, 180);
    (home, pty)
}

fn statusline_requests(server: &TestServer) -> ((CapturedRequest, Value), (CapturedRequest, Value)) {
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

fn statusline_reply_usage(request: CapturedRequest, protocol: &str, text: &str) {
    let mut stream = request.start_sse().unwrap();
    streaming_delta(&mut stream, protocol, text);
    context_tui_stream_usage(&mut stream, protocol, text);
    stream.finish();
}

#[test]
fn statusline_out_accumulates_session_usage_across_requests_both_protocols() {
    let protocol = "response";
        let fixture = server();
        let (_home, mut pty) = statusline_start(&fixture, protocol, false);
        pty.input("live out task\r");
        let ((main, _), (title, _)) = statusline_requests(&fixture);
        statusline_reply_usage(title, protocol, "实时输出");
        let mut stream = main.start_sse().unwrap();
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
        pty.wait_for("新会话清空累计输出并隐藏无数据指标行", |screen| {
            let text = screen.contents();
            !text.contains('\u{f04e2}') && !text.contains('\u{e62a}')
        });
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
}

#[test]
fn task_status_title_parallel_fast_both_protocols_cost_and_history_isolation() {
    let protocol = "response";
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
        assert_eq!(title.target(), api_path(title_protocol));
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
        pty.wait_for("新会话清空标题、费用和累计输出并隐藏无数据指标行", |screen| {
            let text = screen.contents();
            !text.contains('\u{e62a}')
                && !text.contains('\u{f04e2}')
                && !text.contains('\u{f09a9}')
                && !text.contains('\u{f01c1}')
                && !text.contains('\u{f051b}')
        });
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
}

