// The session tree uses a real PTY and the dual-protocol mock provider; inline selection inside the
// message area: moving the candidate only moves the dashed frame, and output/request waits are the
// processing barriers.
const TREE_TUI_UP: &str = "\x1b[A";
const TREE_TUI_DOWN: &str = "\x1b[B";
const TREE_TUI_LEFT: &str = "\x1b[D";
const TREE_TUI_RIGHT: &str = "\x1b[C";
const TREE_TUI_HOME: &str = "\x1b[H";
const TREE_TUI_END: &str = "\x1b[F";

// Open the tree picker: wait for the bottom tips, the actual message being visible and loading being
// gone, rather than for a popup title.
fn tree_tui_open(pty: &mut Pty, expected: &str) {
    pty.input("/tree\r");
    pty.wait_for("tree picker is ready", |screen| {
        let text = screen.contents();
        text.contains("user turns")
            && text.contains("rewind")
            && !text.contains("Loading session tree")
            && text.contains(expected)
    });
}

/// Send a pair of real Esc keys to open the inline tree picker.
///
/// The terminal decodes two adjacent ESC bytes into a single Esc, so the presses need a read gap in
/// between; the 500ms double-click window may also be missed when the machine is busy, so the tree
/// itself is the processing barrier and the whole pair is resent when it does not appear.
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
        assert!(Instant::now() < deadline, "double Esc did not open the inline tree picker");
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

// Read the actual text inside the currently visible dashed frame; return an empty string when no
// outer frame is visible.
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

// For assertions without reflow: the whole rendered row containing the target text, compared after
// dropping the dashed-frame characters.
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

// Select a candidate with an explicit key sequence (derived from the linear effective history order)
// and confirm the expected text appears inside the dashed frame; no search string is pasted.
fn tree_tui_select(pty: &mut Pty, keys: &[&str], expected: &str) {
    for key in keys {
        pty.input(key);
    }
    pty.wait_for("target text appears in the candidate frame", |screen| {
        tree_tui_selected_text(screen).contains(expected)
    });
}

// Enter after selecting: when the candidate is the current leaf it cancels and closes, otherwise it
// commits the navigation and returns to the editor.
fn tree_tui_rewind(pty: &mut Pty, keys: &[&str], expected: &str) {
    tree_tui_select(pty, keys, expected);
    pty.input("\r");
    pty.wait_for("rewind completes and returns to the editor", |screen| {
        input_is_ready(screen) && !screen.contents().contains("user turns")
    });
}

// Build an already-rewound session fixture: after two rounds A and B, rewind to before B's user
// message (the effective history keeps only A), commit the navigation state synchronously and exit
// immediately, without any model request. Returns HOME and the single session ID for the restart,
// `/resume` and `-c` restore tests to reuse.
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
    // The rewind already committed the full state synchronously; exit right after confirming, without
    // sending a follow-up request.
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

        // Restart immediately after the rewind: without sending a new request only the effective
        // history A is restored.
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
        // b no longer switches to the old branch; Home/End and user jumps can only land inside the
        // effective chain.
        tree_tui_select(&mut pty, &["b", TREE_TUI_HOME], "tree-rewind-first-question");
        pty.text("[current] [all] 1/2");
        tree_tui_assert_abandoned_invisible(pty.parser.screen(), &abandoned);
        tree_tui_select(&mut pty, &["b", TREE_TUI_END], "tree-rewind-first-answer");
        pty.text("[current] [all] 2/2");
        tree_tui_assert_abandoned_invisible(pty.parser.screen(), &abandoned);
        pty.input("\x1b");
        pty.ready();
        // Submit C: the actual request user sequence is exactly [A, C], the abandoned B never enters
        // the request.
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
        // Open the tree again: only the effective chain A/C exists, and b and jumps still cannot reveal B.
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
    // Two consecutive complete Escape inputs; the tree picker appearing is itself the event barrier.
    tree_tui_double_escape(&mut pty, "tree-answer");
    // Uniform indentation: every message body starts after column 1 and only the candidate group has
    // an outer frame.
    {
        let screen = pty.parser.screen();
        let (seed_row, seed_col) =
            context_tui_locate(screen, "tree-seed").expect("user message is visible");
        assert_eq!(seed_col, 3, "unselected user groups are also indented one column as a whole");
        assert_eq!(screen.cell(seed_row, 0).unwrap().contents(), " ");
        let (answer_row, answer_col) =
            context_tui_locate(screen, "tree-answer").expect("assistant message is visible");
        assert_eq!(answer_col, 3, "the selected assistant group body is indented the same way");
        assert_eq!(screen.cell(answer_row, 0).unwrap().contents(), "╎");
        let (rows, _) = screen.size();
        let tops = (0..rows)
            .filter(|&row| {
                screen
                    .cell(row, 0)
                    .is_some_and(|cell| cell.contents() == "╭")
            })
            .count();
        assert_eq!(tops, 1, "only one outer frame exists at a time");
    }
    let wrapped = tree_tui_row(pty.parser.screen(), "reasonably");
    // Up/down only move the frame: the body rows and wrapping stay unchanged.
    tree_tui_select(&mut pty, &[TREE_TUI_UP], "tree-seed");
    assert_eq!(tree_tui_row(pty.parser.screen(), "reasonably"), wrapped);
    tree_tui_select(&mut pty, &[TREE_TUI_DOWN], "tree-answer");
    assert_eq!(tree_tui_row(pty.parser.screen(), "reasonably"), wrapped);
    // Home/End on the first and last candidates.
    tree_tui_select(&mut pty, &[TREE_TUI_HOME], "tree-seed");
    tree_tui_select(&mut pty, &[TREE_TUI_END], "tree-answer");
    // f cycles the three levels and the status line shows the level and position.
    pty.input("f");
    pty.text("[no-tools] 2/2");
    pty.input("f");
    pty.text("[user] 1/1");
    pty.input("f");
    pty.text("[all] 1/2");
    // ← jumps to a strictly earlier user in effective-history order; → has no later user, so the
    // candidate does not move.
    tree_tui_select(&mut pty, &[TREE_TUI_END, TREE_TUI_LEFT], "tree-seed");
    tree_tui_select(&mut pty, &[TREE_TUI_RIGHT], "tree-seed");
    // After a narrow-screen resize the outer frame and candidate are still there.
    pty.resize(12, 40);
    tree_tui_select(&mut pty, &[TREE_TUI_HOME], "tree-seed");
    pty.resize(30, 100);
    tree_tui_select(&mut pty, &[TREE_TUI_HOME], "tree-seed");
    // After restoring the original width the wrapping matches the one seen on entry.
    assert_eq!(tree_tui_row(pty.parser.screen(), "reasonably"), wrapped);
    // A single Esc exits.
    pty.input("\x1b");
    pty.ready();
    pty.wait_for("a single Esc closes the tree picker", |screen| {
        !screen.contents().contains("user turns")
    });
    // A non-empty draft cannot open the tree picker with double Esc: the following paste still goes to
    // the editor.
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
        "a non-empty draft must not open the tree picker"
    );
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}

