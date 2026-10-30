use super::*;

#[test]
fn tui_shortcuts_preserve_drafts_and_send_selected_effort_to_both_apis() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        // 旧版设置夹具：think_level 由一次性迁移搬进 agent.db，其余字段留在 settings.yml。
        let settings = r#"{"think_level":"high","rate_limit_retry_budget_seconds":0}"#;
        let models =
            json!({"providers":{"fixture": slash_provider(&server, "fixture", protocol, json!({
                "primary": {"model":"first-model"},
                "secondary": {"model":"second-model"}}))}})
            .to_string();
        std::fs::write(config_path(dir.path(), "settings.yml"), settings).unwrap();
        std::fs::write(config_path(dir.path(), "models.yml"), &models).unwrap();
        let mut pty = Pty::start_with_existing_files(dir.path());
        pty.ready_effort("high");
        pty.input("草稿🙂\x1b[Z");
        pty.ready_effort("xhigh");
        pty.draft("草稿🙂");
        assert!(!pty.parser.screen().contents().contains("Thinking level:"));

        pty.function_key('p');
        pty.text("fixture/secondary");
        pty.model_search("");
        pty.input("\x1b[B\t");
        pty.model_search("fixture/secondary");
        pty.input("\r");
        pty.ready_model("fixture/secondary");
        pty.draft("草稿🙂");
        pty.input("\x1bOP");
        pty.text("Shift + Tab");
        pty.draft("草稿🙂");
        assert!(server.try_recv().unwrap().is_none());

        let path = if protocol == "response" {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        };
        pty.input("\r");
        let (request, body) = receive_at(&server, path);
        super::super::think::assert_level(&body, protocol, "xhigh");
        assert_eq!(body["model"], "second-model");
        let messages = if protocol == "response" {
            &body["input"]
        } else {
            &body["messages"]
        };
        slash_assert_message(
            messages.as_array().unwrap().last().unwrap(),
            "user",
            "草稿🙂",
        );
        pty.input("busy-中\x1b[Z");
        pty.function_key('p');
        pty.model_search("");
        pty.input("\x1b");
        pty.draft("busy-中");
        pty.input("!");
        pty.draft("busy-中!");
        pty.wait_for(
            "忙碌时保持思考级别且模型面板取消后恢复草稿",
            |screen| {
                input_is_ready(screen)
                    && screen
                        .contents()
                        .lines()
                        .last()
                        .is_some_and(|line| line.contains(" xhi ·"))
            },
        );
        pty.input("\x7f");
        pty.draft("busy-中");
        super::super::think::finish_request(request, protocol);
        pty.text("Session:");
        pty.ready_effort("xhigh");
        pty.input("\x01\x0b\x1b[Z");
        pty.ready_effort("max");
        pty.input("\x1b[Z");
        pty.ready_effort("no think");
        pty.input("none request\r");
        let (request, body) = receive_at(&server, path);
        super::super::think::assert_level(&body, protocol, "none");
        super::super::think::finish_request(request, protocol);
        pty.ready_effort("no think");
        pty.input("\x03");
        pty.finish();
        // 迁移后 settings.yml 只保留无关声明；快捷切换不重新写回旧字段。
        let remaining: Value = serde_yaml::from_str(
            &std::fs::read_to_string(config_path(dir.path(), "settings.yml")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            remaining["rate_limit_retry_budget_seconds"].as_u64(),
            Some(0),
            "{remaining}"
        );
        assert!(remaining.get("think_level").is_none(), "{remaining}");
        // 会话级 effort 切换不改默认值；默认仍是一次性迁移导入的 high。
        let database = rusqlite::Connection::open_with_flags(
            config_path(dir.path(), "agent.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert_eq!(
            database
                .query_row(
                    "SELECT default_model, think_level FROM preferences WHERE id = 1",
                    [],
                    |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
                )
                .unwrap(),
            (None, "high".to_string())
        );
        no_more(&server);
    }
}

#[test]
fn tui_think_level_updates_status_and_rejects_changes_while_busy() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut pty = Pty::start(&server, dir.path());
    pty.ready();
    pty.input("/effort ");
    pty.text("Highest level the model supports");
    pty.draft("/effort");
    assert!(server.try_recv().unwrap().is_none());
    pty.input("\u{1b}[B\u{1b}[B\u{1b}[B\u{1b}[B\r");
    pty.wait_for("思考级别更新到状态栏", |screen| {
        input_is_ready(screen)
            && screen
                .contents()
                .lines()
                .last()
                .is_some_and(|line| line.contains(" xhi ·"))
    });
    pty.input("first\r");
    let (request, body) = pty.request(&server);
    assert_eq!(body["reasoning_effort"], "xhigh");
    pty.input("/effort low\r!");
    pty.draft("/effort low!");
    pty.input("\x7f");
    pty.draft("/effort low");
    pty.wait_for("忙碌时保留原级别", |screen| {
        input_is_ready(screen)
            && screen
                .contents()
                .lines()
                .last()
                .is_some_and(|line| line.contains(" xhi ·"))
    });
    final_reply(request, "first answer");
    pty.text("first answer");
    // The draft's menu appears only after Completed, unlike the input
    // border/status which also remain visible while the worker is busy.
    pty.text("Light thinking");
    pty.draft("/effort low");
    pty.input("\r");
    pty.wait_for("完成后切换到 low", |screen| {
        input_is_ready(screen)
            && screen
                .contents()
                .lines()
                .last()
                .is_some_and(|line| line.contains("low"))
    });
    pty.input("second\r");
    let (request, body) = pty.request(&server);
    assert_eq!(body["reasoning_effort"], "low");
    final_reply(request, "second answer");
    pty.text("second answer");
    pty.input("/effort ");
    pty.text("Light thinking (current)");
    pty.input("\x01\x0b");
    pty.draft("");
    // low was already exercised above; each transition now changes the
    // displayed level, so readiness cannot match a stale status value.
    for level in ["medium", "high", "xhigh", "max"] {
        pty.input(&format!("/effort {level}\r"));
        pty.ready_effort(level);
    }
    pty.input("\u{3}");
    pty.finish();
}

#[test]
fn tui_statusline_refreshes_git_without_losing_draft() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "--initial-branch=fixture"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(dir.path().join(".gitignore"), ".pp/\n").unwrap();
    assert!(
        Command::new("git")
            .args(["add", ".gitignore"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    );
    let mut pty = Pty::start(&server, dir.path());
    pty.wait_for("model, effort and Git footer", |screen| {
        screen.contents().lines().last().is_some_and(|line| {
            line.contains("fixture/primary")
                && line.contains("\u{f0766} off")
                && line.contains("\u{f126} fixture *1")
        })
    });
    pty.input("draft-中");
    pty.draft("draft-中");
    std::fs::write(dir.path().join("new-file"), "draft").unwrap();
    pty.wait_for("background Git refresh", |screen| {
        screen
            .contents()
            .lines()
            .last()
            .is_some_and(|line| line.contains("\u{f126} fixture *1 ?1"))
    });
    pty.draft("draft-中");
    pty.resize(4, 20);
    pty.resize(30, 100);
    pty.draft("draft-中");
    pty.text("\u{f126} fixture *1 ?1");
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_mouse_wheel_scrolls_history_and_resumes_following() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(config_path(dir.path(), "settings.yml"), fixture_settings()).unwrap();
    std::fs::write(
        config_path(dir.path(), "models.yml"),
        response_models(&server, Some("completion")),
    )
    .unwrap();
    let mut pty = Pty::start_with_existing_files(dir.path());
    pty.ready();
    pty.input("wheel task\r");
    let (request, _) = receive_at(&server, "/v1/chat/completions");
    let mut stream = SseReply::new(request);
    let mut full = format!(
        "```text\n{}",
        (0..60)
            .map(|i| format!("line-{i:02}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    streaming_delta(&mut stream, "completion", &full);
    pty.text("line-59");
    pty.input("draft-中");
    pty.draft("draft-中");
    pty.input("\x1b[<64;10;5M\x1b[<64;10;5M");
    pty.wait_for("wheel-scrolled history", |screen| {
        let text = screen.contents();
        text.contains("line-53") && !text.contains("line-59")
    });
    let old_view: Vec<String> = pty
        .parser
        .screen()
        .contents()
        .lines()
        .take(25)
        .map(str::to_owned)
        .collect();
    streaming_delta(&mut stream, "completion", "\nnew-a\nnew-b");
    full.push_str("\nnew-a\nnew-b");
    streaming_complete(&mut stream, "completion", &full);
    stream.finish();
    pty.ready();
    assert_eq!(
        pty.parser
            .screen()
            .contents()
            .lines()
            .take(25)
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        old_view
    );
    pty.draft("draft-中");
    pty.input("\x1b[<65;10;5M\x1b[<65;10;5M\x1b[<65;10;5M\x1b[<65;10;5M");
    pty.text("new-b");
    pty.draft("draft-中");
    pty.input("\x7f\x7f\x7f\x7f\x7f\x7f\x7f");
    pty.input("follow task\r");
    let (request, _) = receive_at(&server, "/v1/chat/completions");
    let mut stream = SseReply::new(request);
    streaming_delta(&mut stream, "completion", "follow-visible");
    pty.text("follow-visible");
    streaming_complete(&mut stream, "completion", "follow-visible");
    stream.finish();
    pty.ready();
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}
