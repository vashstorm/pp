// 会话树使用真实 PTY 和双协议 mock provider；消息区内联选择：候选移动只挪
// 虚线框，等待输出与请求作处理栅栏。
const TREE_TUI_UP: &str = "\x1b[A";
const TREE_TUI_DOWN: &str = "\x1b[B";
const TREE_TUI_LEFT: &str = "\x1b[D";
const TREE_TUI_RIGHT: &str = "\x1b[C";
const TREE_TUI_HOME: &str = "\x1b[H";
const TREE_TUI_END: &str = "\x1b[F";

#[test]
fn tree_tui_startup_double_escape_keeps_editor_without_screen_switch() {
    for protocol in ["completion", "response"] {
        let server = Server::http("127.0.0.1:0").unwrap();
        let (_home, mut pty) = context_tui_start(&server, protocol, 100_000, 80);
        pty.drain();
        let before = pty.raw.len();
        let cursor = pty.parser.screen().cursor_position();
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
        context_tui_paste(&mut pty, "焦点栅栏");
        pty.input("\x01\x0b");
        pty.draft("");
        let output = String::from_utf8_lossy(&pty.raw[before..]);
        assert!(!output.contains("\x1b[?1049h"), "空会话不应进入备用屏幕");
        assert!(!output.contains("Loading session tree"));
        let screen = pty.parser.screen();
        assert!(input_is_ready(screen));
        assert!(!screen.alternate_screen());
        assert!(!screen.hide_cursor());
        assert_eq!(screen.cursor_position(), cursor);
        context_tui_paste(&mut pty, "保留输入焦点");
        pty.draft("保留输入焦点");
        no_more(&server);
        pty.input("\x03");
        pty.finish();
    }
}

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

// 显式按键序列选择候选（按线性有效历史顺序推导），确认虚线框内出现期望
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

// 生成已回退会话 fixture：A、B 两轮后回退到 B 用户消息之前（有效历史只剩
// A），导航 state 同步提交后立即退出，期间不发任何模型请求。返回 HOME 与
// 唯一会话 ID，供重启、/resume 与 -c 恢复类测试复用。
fn tree_tui_rewound_session(server: &Server, protocol: &str) -> (tempfile::TempDir, String) {
    let (home, mut pty) = context_tui_start(server, protocol, 100_000, 80);
    context_tui_exchange(
        &mut pty,
        server,
        protocol,
        "tree-rewind-first-question",
        "tree-rewind-first-answer",
    );
    context_tui_exchange(
        &mut pty,
        server,
        protocol,
        "tree-rewind-second-question",
        "tree-rewind-second-answer",
    );
    tree_tui_open(&mut pty, "tree-rewind-second-answer");
    tree_tui_rewind(&mut pty, &[TREE_TUI_UP], "tree-rewind-second-question");
    // 回退已同步提交完整 state；确认后立即退出，不发跟进请求。
    no_more(server);
    pty.input("\x03");
    pty.finish();
    let ids = resume_boundary_session_ids(home.path());
    assert_eq!(ids.len(), 1, "expected exactly one session: {ids:?}");
    (home, ids.into_iter().next().unwrap())
}

fn tree_tui_assert_abandoned_invisible(screen: &vt100::Screen, abandoned: &[&str]) {
    let text = screen.contents();
    for marker in abandoned {
        assert!(
            !text.contains(marker),
            "abandoned message {marker} must not be visible:\n{text}"
        );
    }
}

#[test]
fn tree_tui_rewind_immediately_invalidates_abandoned_messages_after_restart_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = Server::http("127.0.0.1:0").unwrap();
        let (home, id) = tree_tui_rewound_session(&server, protocol);
        let abandoned = ["tree-rewind-second-question", "tree-rewind-second-answer"];

        // 回退后立即重启：不发送新请求也只恢复有效历史 A。
        let mut pty = Pty::start_with_args(home.path(), home.path(), &["--resume", &id]);
        pty.ready();
        {
            let text = pty.parser.screen().contents();
            assert!(text.contains("tree-rewind-first-question"));
            assert!(text.contains("tree-rewind-first-answer"));
        }
        tree_tui_assert_abandoned_invisible(pty.parser.screen(), &abandoned);
        tree_tui_open(&mut pty, "tree-rewind-first-answer");
        pty.text("[current] [all] 2/2");
        tree_tui_assert_abandoned_invisible(pty.parser.screen(), &abandoned);
        // b 不再切换到旧分支；Home/End/用户跳转只能落在有效链内。
        tree_tui_select(&mut pty, &["b", TREE_TUI_HOME], "tree-rewind-first-question");
        pty.text("[current] [all] 1/2");
        tree_tui_assert_abandoned_invisible(pty.parser.screen(), &abandoned);
        tree_tui_select(&mut pty, &["b", TREE_TUI_END], "tree-rewind-first-answer");
        pty.text("[current] [all] 2/2");
        tree_tui_assert_abandoned_invisible(pty.parser.screen(), &abandoned);
        pty.input("\x1b");
        pty.ready();
        // 提交 C：实际请求用户序列精确为 [A, C]，废弃 B 不进入请求。
        context_tui_paste(&mut pty, "tree-rewind-third-question");
        pty.input("\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["tree-rewind-first-question", "tree-rewind-third-question"]
        );
        assert!(!body.to_string().contains("tree-rewind-second"));
        if protocol == "completion" {
            final_reply(request, "tree-rewind-third-answer");
        } else {
            response_final(request, "tree-rewind-third-answer");
        }
        pty.text("tree-rewind-third-answer");
        pty.ready();
        // 再打开树：只有 A/C 有效链，b 与跳转仍不能显露 B。
        tree_tui_open(&mut pty, "tree-rewind-third-answer");
        pty.text("[current] [all] 4/4");
        tree_tui_assert_abandoned_invisible(pty.parser.screen(), &abandoned);
        tree_tui_select(&mut pty, &["b", TREE_TUI_HOME], "tree-rewind-first-question");
        tree_tui_assert_abandoned_invisible(pty.parser.screen(), &abandoned);
        pty.input("\x1b");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn tree_tui_slash_resume_after_rewind_restores_only_valid_history_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = Server::http("127.0.0.1:0").unwrap();
        let (home, id) = tree_tui_rewound_session(&server, protocol);
        let abandoned = ["tree-rewind-second-question", "tree-rewind-second-answer"];

        let mut pty = Pty::start_with_args(home.path(), home.path(), &[]);
        pty.ready();
        resume_boundary_open(&mut pty);
        pty.text("tree-rewind-first-question");
        pty.input("\r");
        resume_history_wait_resumed(
            &mut pty,
            &id,
            &["tree-rewind-first-question", "tree-rewind-first-answer"],
        );
        tree_tui_assert_abandoned_invisible(pty.parser.screen(), &abandoned);
        // TUI /resume 恢复本身不请求模型。
        no_more(&server);
        tree_tui_open(&mut pty, "tree-rewind-first-answer");
        pty.text("[current] [all] 2/2");
        tree_tui_assert_abandoned_invisible(pty.parser.screen(), &abandoned);
        tree_tui_select(&mut pty, &["b", TREE_TUI_HOME], "tree-rewind-first-question");
        tree_tui_assert_abandoned_invisible(pty.parser.screen(), &abandoned);
        pty.input("\x1b");
        pty.ready();
        // 恢复后的实际请求只携带有效历史 A 与新消息 C。
        context_tui_paste(&mut pty, "tree-resume-third-question");
        pty.input("\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["tree-rewind-first-question", "tree-resume-third-question"]
        );
        assert!(!body.to_string().contains("tree-rewind-second"));
        context_tui_final(request, protocol, "tree-resume-third-answer");
        pty.text("tree-resume-third-answer");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn tree_tui_cli_continue_after_rewind_requests_only_valid_history_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = Server::http("127.0.0.1:0").unwrap();
        let (home, id) = tree_tui_rewound_session(&server, protocol);

        // -c 不输入新问题：恢复后立即发出的实际请求只含有效历史 A。
        let cli = Cli::spawn(home.path(), &["-c", &id], &[]);
        let (request, body) = receive_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["tree-rewind-first-question"]
        );
        assert!(!body.to_string().contains("tree-rewind-second"));
        if protocol == "completion" {
            final_reply(request, "tree-continue-answer");
        } else {
            response_final(request, "tree-continue-answer");
        }
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"tree-continue-answer\n");
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
                    "当前有效历史不应包含已放弃的消息"
                );
            }
            if index < 2 {
                tree_tui_rewind(&mut pty, &[TREE_TUI_UP], &question);
                pty.draft(&question);
                pty.input("\x01\x0b");
                pty.draft("");
            }
        }
        // b 不再暴露旧分支；Home/End/用户跳转只能在有效链 kept + attempt-2
        // 内移动，废弃的 attempt-0/1 不可见也不可选。
        for (keys, expected, status) in [
            (&["b", TREE_TUI_HOME][..], "tree-kept-question", "[current] [all] 1/4"),
            (&["b", TREE_TUI_END][..], "tree-attempt-2-answer", "[current] [all] 4/4"),
            (&[TREE_TUI_LEFT][..], "tree-attempt-2-question", "[current] [all] 3/4"),
            (&[TREE_TUI_RIGHT][..], "tree-attempt-2-question", "[current] [all] 3/4"),
        ] {
            tree_tui_select(&mut pty, keys, expected);
            pty.text(status);
            assert!(
                !pty.parser.screen().contents().contains("tree-attempt-0"),
                "b 与跳转不应显露 attempt-0"
            );
            assert!(
                !pty.parser.screen().contents().contains("tree-attempt-1"),
                "b 与跳转不应显露 attempt-1"
            );
        }
        pty.input("\x1b");
        pty.ready();
        // 关闭再打开仍是同一有效链；b 与跳转依旧不能找回旧尝试。
        tree_tui_open(&mut pty, "tree-attempt-2-answer");
        pty.text("[current] [all] 4/4");
        tree_tui_select(
            &mut pty,
            &["b", TREE_TUI_HOME, "b", TREE_TUI_END],
            "tree-attempt-2-answer",
        );
        pty.text("[current] [all] 4/4");
        assert!(!pty.parser.screen().contents().contains("tree-attempt-0"));
        assert!(!pty.parser.screen().contents().contains("tree-attempt-1"));
        pty.input("\x1b");
        pty.ready();
        let ids = resume_boundary_session_ids(home.path());
        pty.input("/quit\r");
        pty.finish();
        // 重启恢复后同样：旧尝试不可见，b/Home/End/用户跳转也调不出。
        let mut pty = Pty::start_with_args(home.path(), home.path(), &["--resume", &ids[0]]);
        pty.resize(50, 100);
        pty.ready();
        tree_tui_open(&mut pty, "tree-attempt-2-answer");
        pty.text("[current] [all] 4/4");
        assert!(!pty.parser.screen().contents().contains("tree-attempt-0"));
        assert!(!pty.parser.screen().contents().contains("tree-attempt-1"));
        tree_tui_select(&mut pty, &["b", TREE_TUI_HOME], "tree-kept-question");
        pty.text("[current] [all] 1/4");
        assert!(!pty.parser.screen().contents().contains("tree-attempt-0"));
        assert!(!pty.parser.screen().contents().contains("tree-attempt-1"));
        tree_tui_select(&mut pty, &[TREE_TUI_END], "tree-attempt-2-answer");
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
    pty.model_search("");
    pty.text("fixture/primary");
    pty.input("\x1b");
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
    // ← 按有效历史顺序跳到严格更早的 user；→ 之后没有更晚的 user，候选不动。
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
            let mut batch = vec![json!({
                "type": "reasoning",
                "id": "tree-tool-reasoning",
                "summary": [],
                "encrypted_content": "tree-opaque-tool-state",
                "status": "completed"
            })];
            batch.extend(tools.iter().map(|(id, name, args)| response_call(id, name, args.clone())));
            response_reply(request, Value::Array(batch));
        }
        let (request, body) = pty.request_at(&server, api_path(protocol));
        let native_input = body["input"].clone();
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(outputs.len(), 2);
        context_tui_final(request, protocol, "tree-tools-finished");
        pty.text("tree-tools-finished");
        pty.ready();
        tree_tui_open(&mut pty, "tree-tools-finished");
        // 线性有效历史：user tools assistant(leaf)。先 Home 对齐组首再一步到
        // tools 组，确认整批工具卡都在同一虚线框内后才回车。
        tree_tui_select(&mut pty, &[TREE_TUI_HOME, TREE_TUI_DOWN], "tree-counter");
        pty.wait_for("工具批次整体框选", |screen| {
            let selected = tree_tui_selected_text(screen);
            selected.contains("$ printf x >> tree-counter")
                && selected.contains("Read · tree-counter")
        });
        pty.input("\r");
        pty.wait_for("跳转完成，回到编辑器", |screen| {
            input_is_ready(screen) && !screen.contents().contains("user turns")
        });
        pty.input("tree-after-tools\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(cancel_tool_outputs(&body, protocol), outputs);
        if protocol == "response" {
            let original = native_input.as_array().unwrap();
            assert_eq!(&body["input"].as_array().unwrap()[..original.len()], original);
            assert!(body.to_string().contains("tree-opaque-tool-state"));
        }
        assert!(!body.to_string().contains("tree-tools-finished"));
        assert_eq!(
            std::fs::read_to_string(home.path().join("tree-counter")).unwrap(),
            "x"
        );
        context_tui_final(request, protocol, "tree-tools-returned");
        pty.text("tree-tools-returned");
        pty.ready();
        tree_tui_open(&mut pty, "tree-tools-returned");
        tree_tui_rewind(&mut pty, &[TREE_TUI_HOME], "tree-run-tools");
        pty.draft("tree-run-tools");
        pty.input("\x01\x0b");
        pty.draft("");
        pty.input("tree-without-tools\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(cancel_user_messages(&body, protocol), ["tree-without-tools"]);
        assert!(cancel_tool_outputs(&body, protocol).is_empty());
        for marker in ["tree-bash", "tree-read", "tree-opaque-tool-state", "tree-tools-returned"] {
            assert!(!body.to_string().contains(marker));
        }
        assert_eq!(std::fs::read_to_string(home.path().join("tree-counter")).unwrap(), "x");
        pty.input("/model ");
        context_tui_final(request, protocol, "tree-tools-discarded");
        pty.text("tree-tools-discarded");
        pty.model_search("");
        pty.text("fixture/primary");
        pty.input("\x1b");
        pty.draft("");
        tree_tui_open(&mut pty, "tree-tools-discarded");
        assert!(!pty.parser.screen().contents().contains("tree-counter"));
        pty.input("\x1b");
        pty.wait_for("discarded tool preview closed", |screen| input_is_ready(screen) && !screen.contents().contains("user turns"));
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
        pty.model_search("");
        pty.text("fixture/primary");
        pty.input("\x1b");
        pty.draft("");
        pty.text("Context compacted:");
        tree_tui_open(&mut pty, "noted context-recent-keep");
        // 线性有效历史：u1 a1 u2 a2 u3 a3(leaf)；单链上三步到 u2。候选组高于
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
        let providers = json!({"providers":{
            "fixture": slash_provider(&server, "fixture", source, json!({"primary": {"model":"fixture-model"}})),
            "other": slash_provider(&server, "other", target, json!({"target": {"model":"target-model"}}))}});
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
        pty.ready_model("other/target");
        pty.input("/effort high\r");
        pty.ready_effort("high");
        tree_tui_open(&mut pty, "tree-cross-second-answer");
        // 线性有效历史：u1 a1 u2 a2(leaf)；两步到第一轮助手节点。
        tree_tui_rewind(
            &mut pty,
            &[TREE_TUI_UP, TREE_TUI_UP],
            "tree-cross-first-answer",
        );
        pty.ready_model("other/target");
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
    // 按固定物理行列比较，避免 contents() 对行尾空白的不同序列化影响等价画面。
    let screen_grid = |screen: &vt100::Screen| {
        let (rows, cols) = screen.size();
        (0..rows)
            .map(|row| tip_columns(screen, row, 0..cols))
            .collect::<Vec<_>>()
    };
    let before = {
        pty.drain();
        screen_grid(pty.parser.screen())
    };
    tree_tui_open(&mut pty, "noted context-recent-keep");
    // 在预览里移动候选、过滤并滚动预览行，全程不应产生模型请求。
    // 线性有效历史：u1 a1 u2 a2 u3 a3(leaf)；三步到 u2。
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
        screen_grid(pty.parser.screen())
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
