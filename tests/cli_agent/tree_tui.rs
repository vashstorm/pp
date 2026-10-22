// 会话树使用真实 PTY 和双协议 mock provider；消息区内联选择：候选移动只挪
// 虚线框，等待输出与请求作处理栅栏。
const TREE_TUI_UP: &str = "\x1b[A";
const TREE_TUI_DOWN: &str = "\x1b[B";
const TREE_TUI_LEFT: &str = "\x1b[D";
const TREE_TUI_RIGHT: &str = "\x1b[C";
const TREE_TUI_HOME: &str = "\x1b[H";
const TREE_TUI_END: &str = "\x1b[F";

// 打开树选择：等待底部 tips、实际消息可见且 loading 消失，而不是弹窗标题。
fn tree_tui_open(pty: &mut Pty, expected: &str) {
    pty.input("/tree\r");
    pty.wait_for("树选择就绪", |screen| {
        let text = screen.contents();
        text.contains("user turns")
            && text.contains("rewind")
            && !text.contains("Loading session tree")
            && text.contains(expected)
    });
}

// 读取当前可见虚线框内的真实文本；没有可见外框时返回空串。
fn tree_tui_selected_text(screen: &vt100::Screen) -> String {
    let (rows, cols) = screen.size();
    let at = |row: u16, col: u16| -> String {
        screen
            .cell(row, col)
            .map(|cell| cell.contents().to_string())
            .unwrap_or_default()
    };
    let framed = |row: u16| {
        matches!(at(row, 0).as_str(), "╭" | "╰" | "╎")
            && matches!(at(row, cols.saturating_sub(1)).as_str(), "╮" | "╯" | "╎")
    };
    let Some(top) = (0..rows).find(|&row| at(row, 0) == "╭" && framed(row)) else {
        return String::new();
    };
    let mut lines = Vec::new();
    let mut row = top;
    loop {
        let mut line = String::new();
        for col in 1..cols.saturating_sub(1) {
            let cell = at(row, col);
            if !matches!(cell.as_str(), "╌" | "╎") {
                line.push_str(&cell);
            }
        }
        lines.push(line.trim_end().to_string());
        row += 1;
        if row >= rows || !framed(row) {
            break;
        }
    }
    lines.join("\n")
}

// 无重排断言用：包含目标文本的整行渲染结果，剔除虚线框字符后比较。
fn tree_tui_row(screen: &vt100::Screen, needle: &str) -> String {
    screen
        .contents()
        .lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("missing row {needle}: {}", screen.contents()))
        .chars()
        .filter(|ch| !matches!(ch, '╌' | '╎' | '╭' | '╮' | '╰' | '╯'))
        .collect::<String>()
        .trim()
        .to_string()
}

// 显式按键序列选择候选（按 active-first DFS 推导），确认虚线框内出现期望
// 文本；不粘贴搜索串。
fn tree_tui_select(pty: &mut Pty, keys: &[&str], expected: &str) {
    for key in keys {
        pty.input(key);
    }
    pty.wait_for("候选虚线框出现目标文本", |screen| {
        tree_tui_selected_text(screen).contains(expected)
    });
}

// 选择后回车：候选是当前 leaf 时取消关闭，否则提交导航并回到编辑器。
fn tree_tui_rewind(pty: &mut Pty, keys: &[&str], expected: &str) {
    tree_tui_select(pty, keys, expected);
    pty.input("\r");
    pty.wait_for("跳转完成，回到编辑器", |screen| {
        input_is_ready(screen) && !screen.contents().contains("user turns")
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
        tree_tui_open(&mut pty, "tree-second-answer");
        // active-first DFS：u1 a1 u2 a2(leaf)。leaf 上一步即第二个 user。
        tree_tui_rewind(&mut pty, &[TREE_TUI_UP], "tree-second-question");
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
        tree_tui_open(&mut pty, "tree-branch-answer");
        // 显式显示全部分支后，旧分支 a2 在 leaf 后两步。
        tree_tui_rewind(&mut pty, &["b", TREE_TUI_DOWN, TREE_TUI_DOWN], "tree-second-answer");
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
        tree_tui_open(&mut pty, "tree-return-answer");
        // 恢复后默认仍只显示当前路径；按 b 才能返回旧分支。
        tree_tui_rewind(
            &mut pty,
            &["b", TREE_TUI_DOWN, TREE_TUI_DOWN],
            "tree-branch-answer",
        );
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
fn tree_tui_repeated_rewinds_hide_abandoned_messages_by_default_and_after_resume() {
    for protocol in ["completion", "response"] {
        let server = server();
        let (home, mut pty) = context_tui_start(&server, protocol, 100_000, 80);
        pty.resize(50, 100);
        context_tui_exchange(
            &mut pty,
            &server,
            protocol,
            "tree-kept-question",
            "tree-kept-answer",
        );
        for index in 0..3 {
            let question = format!("tree-attempt-{index}-question");
            let answer = format!("tree-attempt-{index}-answer");
            context_tui_exchange(&mut pty, &server, protocol, &question, &answer);
            tree_tui_open(&mut pty, &answer);
            pty.text("[current] [all] 4/4");
            for old in 0..index {
                assert!(
                    !pty.parser.screen().contents().contains(&format!("tree-attempt-{old}")),
                    "默认视图不应包含已放弃的消息"
                );
            }
            if index < 2 {
                tree_tui_rewind(&mut pty, &[TREE_TUI_UP], &question);
                pty.draft(&question);
                pty.input("\x01\x0b");
                pty.draft("");
            }
        }
        // 全部分支只在显式切换后出现；关闭再打开恢复默认范围。
        pty.input("b");
        pty.text("[branches] [all] 4/8");
        tree_tui_select(&mut pty, &[TREE_TUI_END], "tree-attempt-1-answer");
        pty.input("\x1b");
        pty.ready();
        tree_tui_open(&mut pty, "tree-attempt-2-answer");
        pty.text("[current] [all] 4/4");
        assert!(!pty.parser.screen().contents().contains("tree-attempt-0"));
        assert!(!pty.parser.screen().contents().contains("tree-attempt-1"));
        pty.input("\x1b");
        pty.ready();
        let ids = resume_boundary_session_ids(home.path());
        pty.input("/quit\r");
        pty.finish();
        let mut pty = Pty::start_with_args(home.path(), home.path(), &["--resume", &ids[0]]);
        pty.resize(50, 100);
        pty.ready();
        tree_tui_open(&mut pty, "tree-attempt-2-answer");
        pty.text("[current] [all] 4/4");
        assert!(!pty.parser.screen().contents().contains("tree-attempt-0"));
        assert!(!pty.parser.screen().contents().contains("tree-attempt-1"));
        pty.input("\x1b");
        pty.ready();
        pty.input("tree-after-repeated-rewinds\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["tree-kept-question", "tree-attempt-2-question", "tree-after-repeated-rewinds"]
        );
        context_tui_final(request, protocol, "tree-repeated-rewinds-done");
        pty.text("tree-repeated-rewinds-done");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn tree_tui_inline_selection_filter_resize_and_cancel_preserve_draft() {
    let server = Server::http("127.0.0.1:0").unwrap();
    let (_home, mut pty) = context_tui_start(&server, "completion", 100_000, 80);
    pty.input("tree-seed\r");
    let (request, _) = pty.request_at(&server, api_path("completion"));
    pty.input("/model ");
    final_reply(request, "tree-answer with a reasonably long reply body so the assistant card wraps across several physical rows in the preview");
    // Match a visible row, not a logical string that spans terminal wrapping.
    pty.text("tree-answer with a reasonably");
    pty.text("fixture/primary");
    pty.input("\x01\x0b");
    pty.draft("");
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
    pty.wait_for("双 Esc 打开内联树选择", |screen| {
        !screen.contents().contains("Loading session tree")
            && tree_tui_selected_text(screen).contains("tree-answer")
    });
    // 统一内缩：所有消息正文都从第 1 列之后开始，只有候选组有外框。
    {
        let screen = pty.parser.screen();
        let (seed_row, seed_col) =
            context_tui_locate(screen, "tree-seed").expect("用户消息可见");
        assert_eq!(seed_col, 3, "未选中的用户组也应整体内缩一列");
        assert_eq!(screen.cell(seed_row, 0).unwrap().contents(), " ");
        let (answer_row, answer_col) =
            context_tui_locate(screen, "tree-answer").expect("助手消息可见");
        assert_eq!(answer_col, 3, "选中的助手组正文同样内缩");
        assert_eq!(screen.cell(answer_row, 0).unwrap().contents(), "╎");
        let (rows, _) = screen.size();
        let tops = (0..rows)
            .filter(|&row| {
                screen
                    .cell(row, 0)
                    .is_some_and(|cell| cell.contents() == "╭")
            })
            .count();
        assert_eq!(tops, 1, "同一时刻只有一个外框");
    }
    let wrapped = tree_tui_row(pty.parser.screen(), "reasonably");
    // 上下移动只挪框：正文行与换行不变。
    tree_tui_select(&mut pty, &[TREE_TUI_UP], "tree-seed");
    assert_eq!(tree_tui_row(pty.parser.screen(), "reasonably"), wrapped);
    tree_tui_select(&mut pty, &[TREE_TUI_DOWN], "tree-answer");
    assert_eq!(tree_tui_row(pty.parser.screen(), "reasonably"), wrapped);
    // Home/End 首尾候选。
    tree_tui_select(&mut pty, &[TREE_TUI_HOME], "tree-seed");
    tree_tui_select(&mut pty, &[TREE_TUI_END], "tree-answer");
    // f 三档轮换，状态行显示档位与位置。
    pty.input("f");
    pty.text("[no-tools] 2/2");
    pty.input("f");
    pty.text("[user] 1/1");
    pty.input("f");
    pty.text("[all] 1/2");
    // ← 按 DFS 顺序跳到严格更早的 user；→ 之后没有更晚的 user，候选不动。
    tree_tui_select(&mut pty, &[TREE_TUI_END, TREE_TUI_LEFT], "tree-seed");
    tree_tui_select(&mut pty, &[TREE_TUI_RIGHT], "tree-seed");
    // 窄屏 resize 后外框与候选仍在。
    pty.resize(12, 40);
    tree_tui_select(&mut pty, &[TREE_TUI_HOME], "tree-seed");
    pty.resize(30, 100);
    tree_tui_select(&mut pty, &[TREE_TUI_HOME], "tree-seed");
    // 恢复原宽度后换行与进入时一致。
    assert_eq!(tree_tui_row(pty.parser.screen(), "reasonably"), wrapped);
    // 一次 Esc 退出。
    pty.input("\x1b");
    pty.ready();
    pty.wait_for("一次 Esc 关闭树选择", |screen| {
        !screen.contents().contains("user turns")
    });
    // 非空草稿不能双 Esc 打开：随后的粘贴仍进入编辑器。
    context_tui_paste(&mut pty, "preserved-tree-draft");
    pty.input("\x1b\x1b");
    context_tui_paste(&mut pty, "!");
    pty.draft("preserved-tree-draft!");
    assert!(
        !pty.parser.screen().contents().contains("user turns"),
        "非空草稿不应打开树选择"
    );
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
        tree_tui_open(&mut pty, "tree-tools-finished");
        // DFS：user tools assistant(leaf)。先 Home 对齐组首再一步到 tools 组，
        // 确认整批工具卡都在同一虚线框内后才回车。
        tree_tui_select(&mut pty, &[TREE_TUI_HOME, TREE_TUI_DOWN], "tree-counter");
        pty.wait_for("工具批次整体框选", |screen| {
            let selected = tree_tui_selected_text(screen);
            selected.contains("$ printf x >> tree-counter")
                && selected.contains("read · tree-counter")
        });
        pty.input("\r");
        pty.wait_for("跳转完成，回到编辑器", |screen| {
            input_is_ready(screen) && !screen.contents().contains("user turns")
        });
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
        tree_tui_open(&mut pty, "noted context-recent-keep");
        // DFS：u1 a1 u2 a2 u3 a3(leaf)；单链上三步到 u2。候选组高于
        // viewport 时对齐组首，首行 filler 可见。
        tree_tui_rewind(
            &mut pty,
            &[TREE_TUI_UP, TREE_TUI_UP, TREE_TUI_UP],
            "context-old-two-0000",
        );
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
            config_path(home.path(), "settings.yml"),
            fixture_settings(),
        )
        .unwrap();
        let providers = json!({"providers":[
            slash_provider(&server, "fixture", source, json!([{"id":"primary","model":"fixture-model"}])),
            slash_provider(&server, "other", target, json!([{"id":"target","model":"target-model"}]))
        ]});
        std::fs::write(
            config_path(home.path(), "models.yml"),
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
        tree_tui_open(&mut pty, "tree-cross-second-answer");
        // DFS：u1 a1 u2 a2(leaf)；两步到第一轮助手节点。
        tree_tui_rewind(
            &mut pty,
            &[TREE_TUI_UP, TREE_TUI_UP],
            "tree-cross-first-answer",
        );
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
        tree_tui_open(&mut pty, "tree-failure-second-answer");
        // f 两次切到 user 过滤（leaf 被隐藏后落到最后一个 user），Home + →
        // 按 tips 定位目标 user，随后回车触发持久化失败。
        tree_tui_select(
            &mut pty,
            &["f", "f", TREE_TUI_HOME, TREE_TUI_RIGHT],
            "tree-failure-second",
        );
        pty.input("\r");
        pty.wait_for("导航持久化失败在树面板显示错误", |screen| {
            screen.contents().contains("user turns")
                && cancel_normalize(screen).contains("persistingsession")
        });
        assert_eq!(std::fs::read(&path).unwrap(), original);
        // 一次 Esc 关闭选择并回到编辑器。
        pty.input("\x1b");
        pty.ready();
        pty.wait_for("一次 Esc 关闭树选择", |screen| {
            !screen.contents().contains("user turns")
        });
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
fn tree_tui_inline_cancel_restores_viewport_and_draft() {
    let server = Server::http("127.0.0.1:0").unwrap();
    let (_home, mut pty) = context_tui_start(&server, "completion", 100_000, 80);
    context_tui_fill_three_exchanges(&mut pty, &server, "completion");
    // 先留在历史位置：PgUp 翻到最早的消息。
    for _ in 0..30 {
        pty.input("\x1b[5~");
    }
    // A visible edit acknowledges every queued PgUp before taking the snapshot.
    // Seeing a history marker alone can match a partially redrawn terminal frame.
    context_tui_paste(&mut pty, "viewport-fence");
    pty.draft("viewport-fence");
    pty.input("\x01\x0b");
    pty.draft("");
    pty.text("context-old-one-0000");
    let before = {
        pty.drain();
        pty.parser.screen().contents()
    };
    tree_tui_open(&mut pty, "noted context-recent-keep");
    // 在预览里移动候选、过滤并滚动预览行，全程不应产生模型请求。
    // DFS：u1 a1 u2 a2 u3 a3(leaf)；三步到 u2。
    tree_tui_select(
        &mut pty,
        &[TREE_TUI_UP, TREE_TUI_UP, TREE_TUI_UP],
        "context-old-two-0000",
    );
    pty.input("ff");
    pty.text("[user]");
    pty.input("\x1b[6~");
    pty.input("\x1b[<65;50;10M");
    // 一次 Esc 退出：普通消息 viewport 与草稿都回到进入前状态。
    pty.input("\x1b");
    pty.wait_for("取消后原消息区域与编辑器恢复", |screen| {
        !screen.contents().contains("user turns")
            && screen.contents().contains("context-old-one-0000")
            && input_is_ready(screen)
    });
    let after = {
        pty.drain();
        pty.parser.screen().contents()
    };
    assert_eq!(
        after, before,
        "Esc 后普通消息 viewport 与草稿应回到进入前状态"
    );
    pty.draft("");
    pty.input("\x03");
    pty.finish();
    no_more(&server);
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
