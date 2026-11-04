use super::*;

fn arguments(transcript: &mut Transcript, tool: &str, index: u32, text: &str) {
    transcript.progress(AgentEvent::ToolArgumentsDelta {
        tool: tool.into(),
        index,
        text: text.into(),
    });
}

fn preview_rows(raw: &str) -> Vec<String> {
    super::super::preview::layout(raw, 200, false, false)
        .iter()
        .map(ToString::to_string)
        .collect()
}

/// 40 行内容的 JSON 参数，每行一个可定位的标记。
fn numbered_lines(count: usize, key: &str) -> String {
    let lines = (1..=count)
        .map(|index| format!("line-{index:02}"))
        .collect::<Vec<_>>()
        .join("\\n");
    format!(r#"{{"{key}":"{lines}"}}"#)
}

#[test]
fn edit_arguments_preview_is_temporary_bounded_and_never_committed_to_native_history() {
    let mut transcript = Transcript::new();
    transcript.begin_run();
    for text in [
        r#"{"path":"demo.rs","edits":[{"oldText":"old","newText":"hel"#,
        "lo中",
    ] {
        arguments(&mut transcript, "edit", 0, text);
    }
    let text = screen(&render(&mut transcript, 100, 15));
    assert!(
        text.contains("preview · not saved") && text.contains("+ hello中"),
        "{text}"
    );
    assert!(transcript.next_native_batch(100, 1, false).is_none());
    assert!(transcript.next_native_batch(100, 1, true).is_none());
    transcript.progress(AgentEvent::ModelAttemptFailed {
        attempt: 1,
        model: String::new(),
        error: String::new(),
    });
    let text = screen(&render(&mut transcript, 100, 15));
    assert!(
        !text.contains("hello中") && !text.contains("not saved"),
        "{text}"
    );
    transcript.progress(AgentEvent::ToolArgumentsDelta {
        tool: "edit".into(),
        index: 0,
        text: format!("{{\"input\":\"{}", "中".repeat(40_000)),
    });
    let index = transcript.argument_previews[&0];
    assert!(transcript.entries[index].text.len() <= 64 * 1024);
    let before = transcript.entries[index].text.clone();
    arguments(&mut transcript, "edit", 0, "SHOULD-NOT-APPEND");
    assert_eq!(transcript.entries[index].text, before);
    assert!(screen(&render(&mut transcript, 100, 15)).contains("preview limit reached"));
    transcript.finish_run("Cancelled", Duration::ZERO, chrono::Local::now());
    let text = screen(&render(&mut transcript, 100, 15));
    assert!(!text.contains("not saved"), "{text}");
    arguments(&mut transcript, "edit", 1, r#"{"input":"discard"#);
    transcript.progress(AgentEvent::AssistantFinished {
        text: String::new(),
    });
    assert!(transcript.argument_previews.is_empty());
}

#[test]
fn arguments_preview_folds_to_tail_only_beyond_forty_rows() {
    // 正好 40 行原样显示，不加提示。
    let rows = preview_rows(&numbered_lines(40, "newText"));
    assert_eq!(rows.len(), 40);
    assert!(!rows.iter().any(|row| row.contains("preview tail")));
    assert_eq!(rows[0], "+ line-01");
    assert_eq!(rows[39], "+ line-40");
    // 41 行只保留末尾 39 行，并在首行提示展开。
    let rows = preview_rows(&numbered_lines(41, "newText"));
    assert_eq!(rows.len(), 40);
    assert_eq!(rows[0], "… preview tail · Ctrl+O to expand");
    assert_eq!(rows[1], "+ line-03");
    assert_eq!(rows[39], "+ line-41");
    // 路径行也计入折叠预算。
    let raw = format!(
        r#"{{"path":"demo.rs",{},"#,
        numbered_lines(40, "newText")
            .trim_start_matches('{')
            .trim_end_matches('}')
    );
    let rows = preview_rows(&raw);
    assert_eq!(rows.len(), 40);
    assert_eq!(rows[0], "… preview tail · Ctrl+O to expand");
    assert_eq!(rows[1], "+ line-02");
    assert_eq!(rows[39], "+ line-40");
    // 展开时显示全部内容。
    let expanded = super::super::preview::layout(&numbered_lines(41, "newText"), 200, true, false);
    assert_eq!(expanded.len(), 41);
}

#[test]
fn arguments_preview_layout_height_stays_folded_for_short_and_long_content() {
    for count in [5, 5000] {
        let mut transcript = Transcript::new();
        transcript.begin_run();
        arguments(
            &mut transcript,
            "write",
            0,
            &numbered_lines(count, "content"),
        );
        let index = transcript.argument_previews[&0];
        let entry = &mut transcript.entries[index];
        entry.ensure_layout(100);
        let rows = super::super::preview::layout(&entry.text, 100, false, false).len();
        // 正文折叠后不超过 40 行，另外固定占一行文件卡片标题。
        assert!(rows <= 40, "{count}: {rows}");
        assert_eq!(entry.layout.as_ref().unwrap().height, rows + 1, "{count}");
    }
}

#[test]
fn write_arguments_preview_streams_path_and_content_without_byte_limit() {
    let mut transcript = Transcript::new();
    transcript.begin_run();
    arguments(
        &mut transcript,
        "write",
        0,
        r#"{"path":"demo.txt","content":"first\nsecond"#,
    );
    arguments(&mut transcript, "write", 0, "-中\"}");
    let text = screen(&render(&mut transcript, 100, 15));
    for needle in [
        "Write:",
        "preview · not saved",
        "File: demo.txt",
        "+ first",
        "+ second-中",
    ] {
        assert!(text.contains(needle), "{needle}: {text}");
    }
    assert!(transcript.next_native_batch(100, 1, false).is_none());
    assert!(transcript.next_native_batch(100, 1, true).is_none());
    // `write` 预览不设字节上限：超过 `edit` 的 64 KiB 也不截断、不显示上限提示。
    arguments(
        &mut transcript,
        "write",
        1,
        r#"{"path":"big.txt","content":""#,
    );
    arguments(&mut transcript, "write", 1, &"中".repeat(40_000));
    let index = transcript.argument_previews[&1];
    assert!(transcript.entries[index].text.len() > 64 * 1024);
    let entry = &mut transcript.entries[index];
    entry.ensure_layout(100);
    let rows = super::super::preview::layout(&entry.text, 100, false, false)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 40);
    assert_eq!(rows[0], "… preview tail · Ctrl+O to expand");
    let text = screen(&render(&mut transcript, 100, 60));
    assert!(!text.contains("preview limit reached"), "{text}");
    assert!(text.contains("preview tail"), "{text}");
    // 路径行按折叠预算被末尾 39 行正文挤出可见区域，但正文仍完整持有。
    assert!(
        transcript.entries[index]
            .text
            .contains(r#""path":"big.txt""#)
    );
    transcript.progress(AgentEvent::AssistantFinished {
        text: String::new(),
    });
    assert!(transcript.argument_previews.is_empty());
    let text = screen(&render(&mut transcript, 100, 15));
    assert!(!text.contains("not saved"), "{text}");
}

#[test]
fn committed_diff_survives_result_and_replay_without_reconstructing_old_logs() {
    let mut transcript = Transcript::new();
    transcript.progress(AgentEvent::ToolStarted {
        id: "edit-1".into(),
        name: "edit".into(),
        arguments: r#"{"path":"demo.txt"}"#.into(),
    });
    transcript.progress(AgentEvent::FileChanged {
        id: "edit-1".into(),
        path: "demo.txt".into(),
        diff: FileDiff::Text {
            deleted: false,
            created: false,
            added: 1,
            removed: 1,
            hunks: vec![pp_core::events::DiffHunk {
                lines: vec![
                    pp_core::events::DiffLine {
                        kind: DiffLineKind::Removed,
                        old_line: Some(1),
                        new_line: None,
                        text: "old\r\n".into(),
                    },
                    pp_core::events::DiffLine {
                        kind: DiffLineKind::Added,
                        old_line: None,
                        new_line: Some(1),
                        text: "old\n".into(),
                    },
                ],
            }],
        },
    });
    transcript.progress(AgentEvent::ToolFinished { id: "edit-1".into(), result: serde_json::json!({"ok":true,"edits_applied":1,"bytes_written":4,"warning":"directory sync warning"}) });
    let buffer = render(&mut transcript, 100, 15);
    let text = screen(&buffer);
    assert!(text.contains("+1/-1"), "{text}");
    assert!(text.contains("CRLF") && text.contains("LF"), "{text}");
    assert!(text.contains("directory sync warning"), "{text}");
    assert!(
        buffer
            .content
            .iter()
            .any(|cell| cell.symbol() == "o" && cell.fg == palette::EDIT_REMOVED)
    );
    assert!(
        buffer
            .content
            .iter()
            .any(|cell| cell.symbol() == "o" && cell.fg == palette::EDIT_ADDED)
    );
    let mut old = Transcript::new();
    old.replay(vec![
        ReplayRecord::Event(AgentEvent::ToolStarted {
            id: "old".into(),
            name: "write".into(),
            arguments: r#"{"path":"old.txt","content":"not a real committed diff"}"#.into(),
        }),
        ReplayRecord::Event(AgentEvent::ToolFinished {
            id: "old".into(),
            result: serde_json::json!({"ok":true,"bytes_written":28}),
        }),
    ]);
    let old_screen = screen(&render(&mut old, 100, 12));
    assert!(
        old_screen.contains("not a real committed diff"),
        "{old_screen}"
    );
    assert!(!old_screen.contains("+28 -0"));
}

#[test]
fn edit_card_reference_layout_colors_inline_changes_and_narrow_widths() {
    use pp_core::events::DiffHunk;
    use pp_core::events::DiffLine;
    let mut transcript = Transcript::new();
    tool(
        &mut transcript,
        "edit",
        "edit",
        json!({"path":"src/agent/read_tests.rs"}),
    );
    transcript.progress(AgentEvent::FileChanged {
            id: "edit".into(),
            path: "src/agent/read_tests.rs".into(),
            diff: FileDiff::Text { deleted: false, created: false, added: 2, removed: 1,
            hunks: vec![DiffHunk { lines: vec![
                DiffLine { kind: DiffLineKind::Context, old_line: Some(162), new_line: Some(162), text: "        let document = crate::plan::PlanDocument::new().unwrap();\n".into() },
                DiffLine { kind: DiffLineKind::Added, old_line: None, new_line: Some(163), text: "        let execute_workflow = crate::plan::WorkflowState::default();\n".into() },
                DiffLine { kind: DiffLineKind::Context, old_line: Some(163), new_line: Some(164), text: "        for workflow in [\"execute\", \"plan\", \"review\"] {\n".into() },
                DiffLine { kind: DiffLineKind::Removed, old_line: Some(165), new_line: None, text: "                \"execute\" => crate::tools::ToolPolicy::Execute,\n".into() },
                DiffLine { kind: DiffLineKind::Added, old_line: None, new_line: Some(166), text: "                \"execute\" => crate::tools::ToolPolicy::Execute { workflow: &execute_workflow },\n".into() },
            ] }], },
        });
    transcript.progress(AgentEvent::ToolFinished {
        id: "edit".into(),
        result: json!({"ok":true}),
    });
    let buffer = render(&mut transcript, 130, 12);
    let text = screen(&buffer);
    let rows: Vec<_> = text.lines().collect();
    assert!(
        rows[0].starts_with("╭─") && rows[0].ends_with('╮'),
        "{text}"
    );
    assert!(
        rows[0].contains("Edit:") && rows[0].contains("src/agent/read_tests.rs ⟨+2/-1⟩"),
        "{text}"
    );
    assert!(rows[1].contains("162│        let document"), "{text}");
    assert!(
        rows[2].contains("+163│········let execute_workflow"),
        "{text}"
    );
    assert!(rows[4].contains("-165│"), "{text}");
    assert!(rows[6].starts_with('╰') && rows[6].ends_with('╯'), "{text}");
    assert_eq!(buffer[(0, 0)].fg, palette::EDIT_BORDER);
    assert_eq!(buffer[(4, 0)].fg, palette::EDIT_TITLE);
    assert_eq!(buffer[(14, 1)].fg, palette::EDIT_KEYWORD);
    assert_eq!(buffer[(6, 2)].fg, palette::EDIT_ADDED);
    assert_eq!(buffer[(6, 2)].bg, palette::EDIT_BG);
    let highlighted: String = (0..130)
        .filter(|&x| buffer[(x, 5)].bg == palette::EDIT_ADDED)
        .map(|x| buffer[(x, 5)].symbol())
        .collect();
    assert_eq!(highlighted, " { workflow: &execute_workflow }");
    assert!(!text.contains(" · ok"));
    for width in [1, 2, 3, 4, 8, 20, 60, 130] {
        let buffer = render(&mut transcript, width, 100);
        if width >= 8 {
            let top = (0..buffer.area.height)
                .find(|&y| buffer[(0, y)].symbol() == "╭")
                .unwrap();
            assert_eq!(buffer[(width - 1, top)].symbol(), "╮");
            assert_eq!(buffer[(width - 1, top + 1)].symbol(), "│");
        }
    }
}

#[test]
fn diff_preview_caps_hunks_and_wrapped_rows_but_full_mode_keeps_tail() {
    let make_line = |number, text| pp_core::events::DiffLine {
        kind: DiffLineKind::Added,
        old_line: None,
        new_line: Some(number),
        text,
    };
    let diff = FileDiff::Text {
        deleted: false,
        created: false,
        added: 9,
        removed: 0,
        hunks: (1..=9)
            .map(|i| pp_core::events::DiffHunk {
                lines: vec![make_line(i, format!("hunk-{i}\n"))],
            })
            .collect(),
    };
    let collapsed = diff_layout(&diff, 100, false)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(collapsed.contains("hunk-8") && !collapsed.contains("hunk-9"));
    assert!(
        diff_layout(&diff, 100, true)
            .iter()
            .any(|row| row.to_string().contains("hunk-9"))
    );
    let diff = FileDiff::Text {
        deleted: false,
        created: false,
        added: 1,
        removed: 0,
        hunks: vec![pp_core::events::DiffHunk {
            lines: vec![make_line(1, format!("{}tail\n", "x".repeat(2000)))],
        }],
    };
    let rows = diff_layout(&diff, 30, false);
    let marker = rows
        .iter()
        .position(|row| row.to_string().contains("more lines"))
        .unwrap();
    assert_eq!(marker, 41);
    assert!(!rows.iter().any(|row| row.to_string().contains("tail")));
    assert!(
        diff_layout(&diff, 30, true)
            .iter()
            .any(|row| row.to_string().contains("tail"))
    );
}

#[test]
fn write_preview_preserves_committed_diff_and_warning_without_rereading_files() {
    let mut transcript = Transcript::new();
    let content = (0..20)
        .map(|i| format!("saved-{i:02}\n"))
        .collect::<String>();
    tool(
        &mut transcript,
        "write",
        "write",
        json!({"path":"missing-on-disk","content":content}),
    );
    transcript.progress(AgentEvent::FileChanged {
        id: "write".into(),
        path: "missing-on-disk".into(),
        diff: FileDiff::Unavailable {
            reason: "Previous content is not UTF-8; text diff unavailable.".into(),
        },
    });
    transcript.progress(AgentEvent::ToolFinished {
        id: "write".into(),
        result: json!({"ok":true,"bytes_written":content.len(),"warning":"directory sync warning"}),
    });
    let collapsed = screen(&render(&mut transcript, 100, 20));
    assert!(collapsed.contains("saved-05") && !collapsed.contains("saved-06"));
    assert!(collapsed.contains("directory sync warning"));
    assert!(matches!(
        transcript.entries[0].body,
        EntryBody::Diff(FileDiff::Unavailable { .. })
    ));
    let mut full = Vec::new();
    transcript
        .write_full_for_test(&mut full, &render(&mut Transcript::new(), 100, 20))
        .unwrap();
    let mut parser = vt100::Parser::new(80, 100, 0);
    parser.process(&full);
    assert!(parser.screen().contents().contains("saved-19"));
}
