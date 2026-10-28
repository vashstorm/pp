use super::*;

#[test]
fn tree_blocks_preserve_markdown_tool_batch_and_geometry_when_selection_moves() {
    let mut transcript = Transcript::new();
    let mut blocks = Vec::new();
    for records in [
        answer(
            "## Markdown\n\nA long **formatted** sentence with 中文 and emoji 😀 that wraps across several physical rows.",
        ),
        answer("ordinary reply"),
        vec![
            ReplayRecord::Event(AgentEvent::AssistantStarted),
            ReplayRecord::Event(AgentEvent::AssistantFinished {
                text: "tool explanation".into(),
            }),
            ReplayRecord::Event(AgentEvent::ToolStarted {
                id: "a".into(),
                name: "read".into(),
                arguments: "{}".into(),
            }),
            ReplayRecord::Event(AgentEvent::ToolFinished {
                id: "a".into(),
                result: json!({"ok":true,"content":"first tool result"}),
            }),
            ReplayRecord::Event(AgentEvent::ToolStarted {
                id: "b".into(),
                name: "edit".into(),
                arguments: r#"{"path":"a.txt"}"#.into(),
            }),
            ReplayRecord::Event(AgentEvent::FileChanged {
                id: "b".into(),
                path: "a.txt".into(),
                diff: FileDiff::Text {
                    created: true,
                    added: 1,
                    removed: 0,
                    hunks: vec![crate::events::DiffHunk {
                        lines: vec![crate::events::DiffLine {
                            kind: DiffLineKind::Added,
                            old_line: None,
                            new_line: Some(1),
                            text: "new diff text".into(),
                        }],
                    }],
                },
            }),
            ReplayRecord::Event(AgentEvent::ToolFinished {
                id: "b".into(),
                result: json!({"ok":true}),
            }),
        ],
    ] {
        let entries = transcript.append_tree_node(records, "");
        blocks.push(TreeBlock {
            entries,
            label: Line::raw("node"),
        });
    }
    let first = tree_render(&mut transcript, &blocks, &[0, 1, 2], Some(0), false, 48, 60);
    let second = tree_render(&mut transcript, &blocks, &[0, 1, 2], Some(2), false, 48, 60);
    let ranges: Vec<_> = transcript
        .tree_layout
        .as_ref()
        .unwrap()
        .groups
        .iter()
        .map(|g| g.rows.clone())
        .collect();
    for range in &ranges {
        for y in range.start + 1..range.end - 1 {
            for x in 1..47 {
                assert_eq!(first[(x, y as u16)], second[(x, y as u16)]);
            }
        }
    }
    let text = screen(&second);
    // 只统计外围候选框；edit 卡片有自己的内嵌边框。
    assert_eq!(
        (0..60).filter(|&y| second[(0, y)].symbol() == "╭").count(),
        1,
        "{text}"
    );
    assert_eq!(
        (0..60).filter(|&y| second[(0, y)].symbol() == "╰").count(),
        1,
        "{text}"
    );
    let tool_rows = ranges[2].clone();
    let tool_text = (tool_rows.start..tool_rows.end)
        .map(|y| {
            (1..47)
                .map(|x| second[(x, y as u16)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        tool_text.contains("tool explanation")
            && tool_text.contains("first tool result")
            && tool_text.contains("new diff text"),
        "{tool_text}"
    );
    for y in tool_rows.start + 1..tool_rows.end - 1 {
        assert_eq!(second[(0, y as u16)].symbol(), "╎");
        assert_eq!(second[(47, y as u16)].symbol(), "╎");
    }
}

#[test]
fn tree_blocks_clip_real_boundaries_scroll_tall_nodes_and_handle_empty_filters() {
    let mut transcript = Transcript::new();
    let entries = transcript.append_tree_node(
        answer(&format!(
            "```text\n{}tail-marker\n```",
            "line\n".repeat(70_000)
        )),
        "",
    );
    let blocks = [TreeBlock {
        entries,
        label: Line::raw("assistant"),
    }];
    let top = tree_render(&mut transcript, &blocks, &[0], Some(0), true, 40, 12);
    assert!(screen(&top).contains('╭'));
    assert!(!screen(&top).contains('╰'));
    transcript.scroll_down(usize::MAX);
    let bottom = tree_render(&mut transcript, &blocks, &[0], Some(0), false, 40, 12);
    assert!(screen(&bottom).contains("tail-marker"));
    assert!(screen(&bottom).contains('╰'));
    assert!(!screen(&bottom).contains('╭'));
    assert!(transcript.offset > usize::from(u16::MAX));
    tree_render(&mut transcript, &blocks, &[0], Some(0), true, 40, 12);
    assert_eq!(transcript.offset, 0);
    for (width, height) in [(1, 1), (2, 2), (5, 3), (40, 12)] {
        tree_render(&mut transcript, &blocks, &[0], Some(0), true, width, height);
    }
    let empty = tree_render(&mut transcript, &blocks, &[], None, true, 60, 12);
    assert!(screen(&empty).contains("Entries hidden by the current filter"));
    assert!(!screen(&empty).contains('╎'));
    let mut fallback = Transcript::new();
    let range = fallback.append_tree_node(answer(""), "");
    let buffer = tree_render(
        &mut fallback,
        &[TreeBlock {
            entries: range,
            label: Line::raw("empty"),
        }],
        &[0],
        Some(0),
        true,
        40,
        12,
    );
    assert!(screen(&buffer).contains("(empty response)"));
}

#[test]
fn tree_narrow_width_omits_decoration_but_still_displays_content() {
    let mut transcript = Transcript::new();
    let entries = transcript.append_tree_node(answer("x"), "");
    let blocks = [TreeBlock {
        entries,
        label: Line::raw("assistant"),
    }];
    tree_render(&mut transcript, &blocks, &[0], Some(0), true, 3, 6);
    for width in [1, 2] {
        let buffer = tree_render(&mut transcript, &blocks, &[0], Some(0), true, width, 6);
        let text = screen(&buffer);
        assert!(text.contains('x'), "{text}");
        assert!(
            !text.contains('╎') && !text.contains('╭') && !text.contains('╰'),
            "{text}"
        );
    }
}
