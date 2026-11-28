// The title-specific tests control the side-channel request directly and do not use the generic
// fixture's automatic title response.
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
        let ((main, _), (title, title_body)) = statusline_requests(&fixture);
        assert!(title_body.get("max_output_tokens").is_none());
        statusline_reply_usage(title, protocol, "live output");
        let mut stream = main.start_sse().unwrap();
        // A 19-byte body; while this request has not terminated, out can only come from the byte estimate.
        streaming_delta(&mut stream, protocol, "statusline-live-out");
        pty.text("statusline-live-out");
        pty.wait_for("out grows in real time with the bytes received while generating", |screen| {
            screen.contents().contains("\u{f04e2} —/7")
        });
        context_tui_stream_usage(&mut stream, protocol, "statusline-live-out");
        stream.finish();
        pty.completed_after("statusline-live-out");
        pty.wait_for("after the first request completes out equals the session total", |screen| {
            screen.contents().contains("\u{f04e2} 12K/300")
        });
        // The second request in the same session: out stacks onto the previous 300 instead of restarting
        // from this request's usage.
        pty.input("second out task\r");
        let ((main, _), (title, _)) = statusline_requests(&fixture);
        statusline_reply_usage(title, protocol, "second task output");
        statusline_reply_usage(main, protocol, "statusline-second-out");
        pty.completed_after("statusline-second-out");
        pty.wait_for("after the second request out accumulates for the whole session", |screen| {
            screen.contents().contains("\u{f04e2} 12K/600")
        });
        pty.input("/new\r");
        pty.wait_for("a new session clears accumulated output and hides the no-data metrics row", |screen| {
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
        pty.input("fix session history sqlite\r");
        // Both requests arrive before any response, proving the title does not block the main request.
        let ((main, body), (title, title_body)) = statusline_requests(&fixture);
        let title_protocol = if protocol == "completion" {
            "response"
        } else {
            "completion"
        };
        assert_eq!(body["model"], "main-model");
        assert_eq!(title_body["model"], "title-model");
        assert_eq!(title_body["max_completion_tokens"], 64);
        assert!(title_body["tools"].is_null());
        assert!(title_body["tool_choice"].is_null());
        assert_eq!(title.target(), api_path(title_protocol));
        assert!(!body.to_string().contains("Generate a short task title"));
        statusline_reply_usage(title, title_protocol, "fix history");
        pty.wait_for("background title and cost arrive before the main reply", |screen| {
            let text = screen.contents();
            text.contains("fix history")
                && text.contains("0.025")
                && text.contains("\u{f04e2} —/—")
        });
        assert!(
            context_tui_speed(pty.parser.screen()).is_none(),
            "the title must not pollute the main request speed"
        );
        statusline_reply_usage(main, protocol, "statusline-main-answer");
        pty.completed_after("statusline-main-answer");
        pty.wait_for("main request and title costs accumulate only once", |screen| {
            screen.contents().contains("0.037")
        });
        let screen = pty.parser.screen();
        let (row, col) = context_tui_locate(screen, "0.037").unwrap();
        assert_eq!(
            screen.cell(row, col).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
        let (title_row, title_col) = context_tui_locate(screen, "\u{f09a9} fix history").unwrap();
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
        pty.wait_for("a new session clears the title, cost and accumulated output and hides the no-data metrics row", |screen| {
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

