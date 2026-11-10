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
        // 运行中切档只登记期望值：状态栏仍是已应用的 xhi，次要状态区显示待生效。
        pty.wait_for(
            "运行中登记待生效档位且保留已应用档位",
            |screen| {
                let text = screen.contents();
                text.contains("pending next request")
                    && text
                        .lines()
                        .last()
                        .is_some_and(|line| line.contains(" xhi ·"))
            },
        );
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
        pty.completed();
        // 边界已到：忙碌期间登记的档位在本次任务结束后应用。
        pty.ready_effort("max");
        pty.input("\x01\x0b/effort none\r");
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
fn tui_effort_and_shift_tab_apply_to_each_continuation_while_busy() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("draft.txt"), "draft body").unwrap();
    std::fs::write(dir.path().join("draft2.txt"), "second body").unwrap();
    let mut pty = Pty::start(&server, dir.path());
    pty.ready();
    pty.input("/effort ");
    pty.text("Highest level the model supports");
    pty.draft("/effort");
    assert!(server.try_recv().unwrap().is_none());
    pty.input("\u{1b}[B\u{1b}[B\u{1b}[B\u{1b}[B\r");
    pty.ready_effort("xhigh");
    pty.input("first\r");
    let (request, body) = pty.request(&server);
    assert_eq!(body["reasoning_effort"], "xhigh");
    // 不回复本轮请求：运行中提交的 `/effort` 只登记期望值，下一个请求必须已在
    // 边界领取新档位，且状态栏在边界前仍显示已应用的 xhi。
    pty.input("/effort none\r");
    pty.wait_for("运行中登记 /effort 待生效档位", |screen| {
        let text = screen.contents();
        text.contains("pending next request")
            && text
                .lines()
                .last()
                .is_some_and(|line| line.contains(" xhi ·"))
    });
    calls_reply(
        request,
        json!([call("effort-one", "read", json!({"path":"draft.txt"}))]),
    );
    let (request, body) = pty.request(&server);
    assert_eq!(body["reasoning_effort"], "none");
    pty.wait_for("边界后状态栏切到 no think", |screen| {
        input_is_ready(screen)
            && screen
                .contents()
                .lines()
                .last()
                .is_some_and(|line| line.contains(" off ·"))
    });
    // Shift + Tab 在同一任务的下一轮输出期间同样只登记期望值。
    pty.input("\u{1b}[Z");
    pty.wait_for("运行中 Shift + Tab 登记待生效档位", |screen| {
        screen.contents().contains("pending next request")
    });
    calls_reply(
        request,
        json!([call("effort-two", "read", json!({"path":"draft2.txt"}))]),
    );
    let (request, body) = pty.request(&server);
    assert_eq!(body["reasoning_effort"], "low");
    final_reply(request, "busy effort done");
    pty.completed_after("busy effort done");
    pty.ready_effort("low");
    // 菜单的“当前”标记跟已应用档位走。
    pty.input("/effort ");
    pty.text("Light thinking (current)");
    pty.input("\x01\x0b");
    pty.draft("");
    pty.input("/effort max\r");
    pty.ready_effort("max");
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
    // 活动行固定占一行，且上方还有一行空白分隔行（252f311 起）。流式结束时
    // 这两行一起收回、转录区变高，所以"上翻视口不跳"只能比较完成前后都属于
    // 转录区的行，否则视口变高露出的后续行会被误判为拉回底部。
    let before: Vec<String> = pty
        .parser
        .screen()
        .contents()
        .lines()
        .map(str::to_owned)
        .collect();
    let activity = before
        .iter()
        .position(|row| row.contains("Responding"))
        .expect("流式状态行应显示在活动区");
    assert!(activity > 1, "上翻后转录区应已有可见行: {before:?}");
    let old_view = before[..activity - 1].to_vec();
    streaming_delta(&mut stream, "completion", "\nnew-a\nnew-b");
    full.push_str("\nnew-a\nnew-b");
    streaming_complete(&mut stream, "completion", &full);
    stream.finish();
    pty.ready();
    let after: Vec<String> = pty
        .parser
        .screen()
        .contents()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        after[..old_view.len()],
        old_view[..],
        "上翻时视口应保持原位"
    );
    assert!(
        !pty.parser.screen().contents().contains("line-59"),
        "完成时不应跳到底部"
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
