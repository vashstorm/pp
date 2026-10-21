// 会话树使用真实 PTY 和双协议 mock provider；等待输出与请求作处理栅栏。
fn tree_tui_open(pty: &mut Pty) {
    pty.input("/tree\r");
    pty.wait_for("会话树加载完成", |screen| {
        screen.contents().contains("Session Tree")
            && !screen.contents().contains("Loading session tree")
    });
}

fn tree_tui_select(pty: &mut Pty, text: &str) {
    let text = if let Some(user) = text.strip_prefix("user ") {
        pty.input("\x0f\x0f");
        pty.text("[user]");
        user
    } else {
        text
    };
    context_tui_paste(pty, text);
    pty.wait_for("树搜索已筛选", |screen| {
        screen.contents().contains(&format!("Search: {text}"))
    });
    pty.input("\r");
    pty.wait_for("跳转完成，回到编辑器", |screen| {
        input_is_ready(screen) && !screen.contents().contains("Session Tree")
    });
}

#[test]
fn tree_tui_branches_switch_without_requests_and_resume_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = Server::http("127.0.0.1:0").unwrap();
        let (home, mut pty) = context_tui_start(&server, protocol, 100_000, 80);
        context_tui_exchange(
            &mut pty,
            &server,
            protocol,
            "tree-original-question",
            "tree-original-answer",
        );
        context_tui_exchange(
            &mut pty,
            &server,
            protocol,
            "tree-second-question",
            "tree-second-answer",
        );
        tree_tui_open(&mut pty);
        tree_tui_select(&mut pty, "user tree-second-question");
        pty.draft("tree-second-question");
        pty.input("\x01\x0b");
        context_tui_paste(&mut pty, "tree-branch-question");
        pty.input("\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["tree-original-question", "tree-branch-question"]
        );
        assert!(!body.to_string().contains("tree-second-answer"));
        if protocol == "completion" {
            final_reply(request, "tree-branch-answer");
        } else {
            response_final(request, "tree-branch-answer");
        }
        pty.text("tree-branch-answer");
        pty.ready();
        tree_tui_open(&mut pty);
        pty.text("tree-second-answer");
        pty.text("tree-branch-answer");
        tree_tui_select(&mut pty, "tree-second-answer");
        context_tui_paste(&mut pty, "tree-after-return");
        pty.input("\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            [
                "tree-original-question",
                "tree-second-question",
                "tree-after-return"
            ]
        );
        assert!(!body.to_string().contains("tree-branch-question"));
        if protocol == "completion" {
            final_reply(request, "tree-return-answer");
        } else {
            response_final(request, "tree-return-answer");
        }
        pty.text("tree-return-answer");
        pty.ready();
        let ids = resume_boundary_session_ids(home.path());
        assert_eq!(ids.len(), 1);
        pty.input("/quit\r");
        pty.finish();
        let mut pty = Pty::start_with_args(home.path(), home.path(), &["--resume", &ids[0]]);
        pty.ready();
        tree_tui_open(&mut pty);
        tree_tui_select(&mut pty, "tree-branch-answer");
        context_tui_paste(&mut pty, "tree-after-restart");
        pty.input("\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            [
                "tree-original-question",
                "tree-branch-question",
                "tree-after-restart"
            ]
        );
        assert!(!body.to_string().contains("tree-second-answer"));
        if protocol == "completion" {
            final_reply(request, "tree-restored-answer");
        } else {
            response_final(request, "tree-restored-answer");
        }
        pty.text("tree-restored-answer");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn tree_tui_double_escape_search_resize_and_cancel_preserve_draft() {
    let server = Server::http("127.0.0.1:0").unwrap();
    let (_home, mut pty) = context_tui_start(&server, "completion", 100_000, 80);
    context_tui_exchange(&mut pty, &server, "completion", "tree-seed", "tree-answer");
    // 连续两个完整 Escape 输入，真实事件解码不借助计时 sleep。
    pty.drain();
    let before = pty.raw.len();
    pty.input("\x1b");
    let deadline = Instant::now() + DEADLINE;
    loop {
        pty.drain();
        if pty.raw.len() > before {
            break;
        }
        assert!(Instant::now() < deadline, "首个 Esc 未处理");
        thread::sleep(Duration::from_millis(10));
    }
    pty.input("\x1b");
    pty.wait_for("双 Esc 打开树", |screen| {
        screen.contents().contains("Session Tree")
    });
    pty.wait_for("树加载完成", |screen| {
        screen.contents().contains("user: tree-seed")
    });
    context_tui_paste(&mut pty, "不存在🙂");
    pty.text("No entries match search");
    pty.input("\x1b");
    pty.wait_for("Esc 清空搜索并保留树", |screen| {
        screen.contents().contains("Session Tree") && screen.contents().contains("user: tree-seed")
    });
    pty.resize(12, 40);
    pty.text("Session Tree");
    pty.input("\x1b");
    pty.ready();
    pty.resize(30, 100);
    context_tui_paste(&mut pty, "preserved-tree-draft");
    pty.input("\x1b\x1b");
    pty.draft("preserved-tree-draft");
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}

#[test]
fn tree_tui_tool_batch_stays_paired_and_never_reexecutes_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = server();
        let (home, mut pty) = context_tui_start(&server, protocol, 100_000, 80);
        pty.input("tree-run-tools\r");
        let (request, _) = pty.request_at(&server, api_path(protocol));
        let tools = [
            (
                "tree-bash",
                "bash",
                json!({"command":"printf x >> tree-counter"}),
            ),
            ("tree-read", "read", json!({"path":"tree-counter"})),
        ];
        if protocol == "completion" {
            calls_reply(
                request,
                Value::Array(
                    tools
                        .iter()
                        .map(|(id, name, args)| call(id, name, args.clone()))
                        .collect(),
                ),
            );
        } else {
            response_reply(
                request,
                Value::Array(
                    tools
                        .iter()
                        .map(|(id, name, args)| response_call(id, name, args.clone()))
                        .collect(),
                ),
            );
        }
        let (request, body) = pty.request_at(&server, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(outputs.len(), 2);
        context_tui_final(request, protocol, "tree-tools-finished");
        pty.text("tree-tools-finished");
        pty.ready();
        tree_tui_open(&mut pty);
        tree_tui_select(&mut pty, "tools bash read");
        pty.input("tree-after-tools\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(cancel_tool_outputs(&body, protocol), outputs);
        assert!(!body.to_string().contains("tree-tools-finished"));
        assert_eq!(
            std::fs::read_to_string(home.path().join("tree-counter")).unwrap(),
            "x"
        );
        context_tui_final(request, protocol, "tree-tools-returned");
        pty.text("tree-tools-returned");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn tree_tui_navigation_before_compaction_restores_native_history_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = server();
        let (home, mut pty) = context_tui_start(&server, protocol, 100_000, 80);
        context_tui_fill_three_exchanges(&mut pty, &server, protocol);
        pty.input("/compact\r");
        let (request, _) = pty.request_at(&server, api_path(protocol));
        pty.input("/model ");
        context_tui_final(request, protocol, "tree-summary-after-compaction");
        pty.text("Context compacted:");
        pty.text("fixture/primary");
        pty.input("\x01\x0b");
        pty.draft("");
        tree_tui_open(&mut pty);
        tree_tui_select(&mut pty, "user context-old-two");
        // 直接退出并从已提交叶子重启，验证压缩前历史无需继续请求也能恢复。
        pty.input("\x03");
        pty.finish();
        let ids = resume_boundary_session_ids(home.path());
        let mut pty = Pty::start_with_args(home.path(), home.path(), &["--resume", &ids[0]]);
        pty.ready();
        pty.input("tree-after-old-history\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert!(body.to_string().contains("context-old-one"));
        assert!(!body.to_string().contains("context-old-two"));
        assert!(!body.to_string().contains("tree-summary-after-compaction"));
        context_tui_final(request, protocol, "tree-old-history-restored");
        pty.text("tree-old-history-restored");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn tree_tui_cross_protocol_navigation_keeps_current_model_and_effort() {
    for (source, target) in [("completion", "response"), ("response", "completion")] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(home.path(), "settings.jsonc"),
            fixture_settings(),
        )
        .unwrap();
        let providers = json!({"providers":[
            slash_provider(&server, "fixture", source, json!([{"id":"primary","model":"fixture-model"}])),
            slash_provider(&server, "other", target, json!([{"id":"target","model":"target-model"}]))
        ]});
        std::fs::write(
            config_path(home.path(), "models.jsonc"),
            providers.to_string(),
        )
        .unwrap();
        let mut pty = Pty::start_with_existing_files(home.path());
        pty.ready();
        context_tui_exchange(
            &mut pty,
            &server,
            source,
            "tree-cross-first",
            "tree-cross-first-answer",
        );
        context_tui_exchange(
            &mut pty,
            &server,
            source,
            "tree-cross-second",
            "tree-cross-second-answer",
        );
        pty.input("/model other/target\r");
        pty.ready_model("target-model");
        pty.input("/effort high\r");
        pty.ready_effort("high");
        tree_tui_open(&mut pty);
        tree_tui_select(&mut pty, "assistant tree-cross-first-answer");
        pty.ready_model("target-model");
        pty.ready_effort("high");
        pty.input("tree-cross-continue\r");
        let (request, body) = pty.request_at(&server, api_path(target));
        assert_eq!(body["model"], "target-model");
        assert_eq!(
            body[if target == "completion" {
                "reasoning_effort"
            } else {
                "reasoning"
            }],
            if target == "completion" {
                json!("high")
            } else {
                json!({"effort":"high","summary":"auto"})
            }
        );
        assert_eq!(
            cancel_user_messages(&body, target),
            ["tree-cross-first", "tree-cross-continue"]
        );
        assert!(!body.to_string().contains("tree-cross-second"));
        context_tui_final(request, target, "tree-cross-done");
        pty.text("tree-cross-done");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn tree_tui_failed_navigation_preserves_branch_and_restarts_from_previous_state() {
    for protocol in ["completion", "response"] {
        let server = server();
        let (home, mut pty) = context_tui_start(&server, protocol, 100_000, 80);
        context_tui_exchange(
            &mut pty,
            &server,
            protocol,
            "tree-failure-first",
            "tree-failure-first-answer",
        );
        context_tui_exchange(
            &mut pty,
            &server,
            protocol,
            "tree-failure-second",
            "tree-failure-second-answer",
        );
        let ids = resume_boundary_session_ids(home.path());
        pty.input("/quit\r");
        pty.finish();
        let path = resume_boundary_journal(home.path(), &ids[0]);
        let original = std::fs::read(&path).unwrap();
        let limit = (original.len() as u64 + 2048).div_ceil(1024);
        let mut pty =
            Pty::start_with_file_limit(home.path(), home.path(), limit, &["--resume", &ids[0]]);
        pty.ready();
        tree_tui_open(&mut pty);
        pty.input("\x0f\x0f");
        pty.text("[user]");
        context_tui_paste(&mut pty, "tree-failure-second");
        pty.text("Search: tree-failure-second");
        pty.input("\r");
        pty.wait_for("导航持久化失败在树面板显示错误", |screen| {
            screen.contents().contains("Session Tree")
                && cancel_normalize(screen).contains("persistingsession")
        });
        assert_eq!(std::fs::read(&path).unwrap(), original);
        pty.input("\x1b");
        pty.text("Search: Type to search");
        pty.input("\x1b");
        pty.ready();
        pty.input("\x03");
        pty.finish();
        let mut pty = Pty::start_with_args(home.path(), home.path(), &["--resume", &ids[0]]);
        pty.ready();
        pty.input("tree-after-failure\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            [
                "tree-failure-first",
                "tree-failure-second",
                "tree-after-failure"
            ]
        );
        context_tui_final(request, protocol, "tree-failure-recovered");
        pty.text("tree-failure-recovered");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn tree_line_mode_is_a_local_command_and_prompt_mode_keeps_literal_text() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let mut cli = Cli::start(&server, home.path(), None);
    cli.input("/tree\n/tree extra\n/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("/tree requires the TUI"));
    assert!(stderr.contains("usage: /tree"));
    no_more(&server);
    let cli = Cli::start(&server, home.path(), Some("/tree"));
    let (request, body) = receive(&server);
    assert_eq!(cancel_user_messages(&body, "completion"), ["/tree"]);
    final_reply(request, "literal-tree-task");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "literal-tree-task"
    );
    no_more(&server);
}
