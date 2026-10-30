use super::*;

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
            created: false,
            added: 1,
            removed: 1,
            hunks: vec![crate::events::DiffHunk {
                lines: vec![
                    crate::events::DiffLine {
                        kind: DiffLineKind::Removed,
                        old_line: Some(1),
                        new_line: None,
                        text: "old\r\n".into(),
                    },
                    crate::events::DiffLine {
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
    use crate::events::{DiffHunk, DiffLine};
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
            diff: FileDiff::Text {
                created: false, added: 2, removed: 1,
                hunks: vec![DiffHunk { lines: vec![
                    DiffLine { kind: DiffLineKind::Context, old_line: Some(162), new_line: Some(162), text: "        let document = crate::plan::PlanDocument::new().unwrap();\n".into() },
                    DiffLine { kind: DiffLineKind::Added, old_line: None, new_line: Some(163), text: "        let execute_workflow = crate::plan::WorkflowState::default();\n".into() },
                    DiffLine { kind: DiffLineKind::Context, old_line: Some(163), new_line: Some(164), text: "        for workflow in [\"execute\", \"plan\", \"review\"] {\n".into() },
                    DiffLine { kind: DiffLineKind::Removed, old_line: Some(165), new_line: None, text: "                \"execute\" => crate::tools::ToolPolicy::Execute,\n".into() },
                    DiffLine { kind: DiffLineKind::Added, old_line: None, new_line: Some(166), text: "                \"execute\" => crate::tools::ToolPolicy::Execute { workflow: &execute_workflow },\n".into() },
                ] }],
            },
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
    let make_line = |number, text| crate::events::DiffLine {
        kind: DiffLineKind::Added,
        old_line: None,
        new_line: Some(number),
        text,
    };
    let diff = FileDiff::Text {
        created: false,
        added: 9,
        removed: 0,
        hunks: (1..=9)
            .map(|i| crate::events::DiffHunk {
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
        created: false,
        added: 1,
        removed: 0,
        hunks: vec![crate::events::DiffHunk {
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
