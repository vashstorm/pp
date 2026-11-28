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

/// 发送一对真实 Esc 打开内联树选择。
///
/// 终端把紧邻的两个 ESC 字节解码成一个 Esc，因此两次按压之间要留出一次读取
/// 间隙；机器繁忙时 500ms 的双击窗口也可能错过，所以用树本身作为处理栅栏，
/// 未出现就重发整对。
fn tree_tui_double_escape(pty: &mut Pty, expected: &str) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        pty.input("\x1b");
        if tree_tui_tree_ready(pty, expected, Duration::from_millis(200)) {
            return;
        }
        pty.input("\x1b");
        if tree_tui_tree_ready(pty, expected, Duration::from_millis(1200)) {
            return;
        }
        assert!(Instant::now() < deadline, "双 Esc 未打开内联树选择");
    }
}

fn tree_tui_tree_ready(pty: &mut Pty, expected: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        pty.drain();
        let screen = pty.parser.screen();
        if !screen.contents().contains("Loading session tree")
            && tree_tui_selected_text(screen).contains(expected)
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
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
fn tree_tui_rewound_session(server: &TestServer, protocol: &str) -> (tempfile::TempDir, String) {
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
    let protocol = "response";
        let server = TestServer::start().unwrap();
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

#[test]
fn tree_tui_inline_selection_filter_resize_and_cancel_preserve_draft() {
    let server = TestServer::start().unwrap();
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
    pty.completed_after("tree-answer with a reasonably");
    // 连续两个完整 Escape 输入；树选择出现本身作为事件处理栅栏。
    tree_tui_double_escape(&mut pty, "tree-answer");
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
    pty.draft("preserved-tree-draft");
    pty.input("\x1b");
    thread::sleep(Duration::from_millis(80));
    pty.input("\x1b");
    thread::sleep(Duration::from_millis(80));
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

