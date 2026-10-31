use super::*;

#[test]
fn mcp_preview_renders_content_and_structured_results_and_expands_details() {
    for structured in [false, true] {
        let mut transcript = Transcript::new();
        tool(
            &mut transcript,
            "arch",
            "mcp__codebase__get_architecture",
            json!({}),
        );
        let data = json!({"total_nodes": 42, "clusters": [{
            "label": "Agent", "members": 12, "top_nodes": ["run", "save"],
            "detail": {"retained": "expanded detail"}
        }]});
        let result = if structured {
            json!({"ok": true, "result": {"structuredContent": data, "content": []}})
        } else {
            json!({"ok": true, "result": {"content": [{"type":"text", "text":data.to_string()}]}})
        };
        transcript.progress(AgentEvent::ToolFinished {
            id: "arch".into(),
            result: result.clone(),
        });
        let collapsed = screen(&render(&mut transcript, 110, 30));
        for expected in [
            "total_nodes: 42",
            "clusters: 1 item",
            "label: Agent",
            "members: 12",
            "Ctrl + O to expand",
        ] {
            assert!(collapsed.contains(expected), "{collapsed}");
        }
        assert!(!collapsed.contains("expanded detail"));
        transcript.toggle_tool_output();
        let expanded = screen(&render(&mut transcript, 110, 30));
        assert!(expanded.contains("expanded detail"), "{expanded}");
        assert_eq!(transcript.entries[0].result.as_ref(), Some(&result));
        transcript.toggle_tool_output();
        assert_eq!(screen(&render(&mut transcript, 110, 30)), collapsed);
    }
}

#[test]
fn tool_folding_is_global_without_layout_and_future_output_inherits_it() {
    let mut transcript = Transcript::with_read_preview(true);
    for name in [
        "bash",
        "bash_task",
        "read",
        "grep",
        "find",
        "mcp__server__tool",
    ] {
        tool(&mut transcript, name, name, json!({}));
        transcript.entries.last_mut().unwrap().text = (0..20)
            .map(|i| format!("{name}-{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
    }
    transcript.toggle_tool_output();
    for entry in &mut transcript.entries {
        let name = entry.tool.clone().unwrap();
        assert!(entry.expanded);
        let text = entry_text(entry, 100);
        assert!(text.contains(&format!("{name}-00")));
        assert_eq!(
            text.contains(&format!("{name}-19")),
            !name.starts_with("mcp__")
        );
    }
    tool(&mut transcript, "live", "bash", json!({"command":"live"}));
    transcript.progress(AgentEvent::ToolOutput {
        id: "live".into(),
        stream: OutputStream::Stdout,
        text: (0..20).map(|i| format!("live-{i:02}\n")).collect(),
    });
    assert!(entry_text(transcript.entries.last_mut().unwrap(), 100).contains("live-00"));
    transcript.toggle_tool_output();
    assert!(transcript.entries.iter().all(|entry| !entry.expanded));
    let text = entry_text(transcript.entries.last_mut().unwrap(), 100);
    assert!(!text.contains("live-00"));
    assert!(text.contains("10 earlier lines") && text.contains("live-19"));
    transcript.progress(AgentEvent::ToolOutput {
        id: "live".into(),
        stream: OutputStream::Stdout,
        text: "live-20\n".into(),
    });
    assert!(entry_text(transcript.entries.last_mut().unwrap(), 100).contains("11 earlier lines"));
    transcript.progress(AgentEvent::ToolFinished {
        id: "live".into(),
        result: json!({"ok":false,"error":"failed\n".repeat(20)}),
    });
    assert_eq!(
        transcript.entries.last().unwrap().status,
        EntryStatus::Failed
    );
    assert!(entry_text(transcript.entries.last_mut().unwrap(), 100).contains("to expand"));
}

#[test]
fn read_group_cache_refreshes_status_and_failure_boundaries() {
    let mut transcript = Transcript::new();
    for id in ["one", "two", "three"] {
        tool(&mut transcript, id, "read", json!({"path":id,"offset":20}));
    }
    let text = screen(&render(&mut transcript, 100, 30));
    assert!(
        text.contains("Read (3)") && text.contains("three:20 · running"),
        "{text}"
    );
    transcript.progress(AgentEvent::ToolFinished {
        id: "one".into(),
        result: json!({"ok":true,"content":"one-body","lines_read":2}),
    });
    let text = screen(&render(&mut transcript, 100, 30));
    assert!(
        text.contains("Read (3)") && text.contains("one:20-21") && !text.contains(" · ok"),
        "{text}"
    );
    transcript.progress(AgentEvent::ToolFinished {
        id: "two".into(),
        result: json!({"ok":false,"error":"second read failure"}),
    });
    let text = screen(&render(&mut transcript, 100, 30));
    for expected in [
        "one:20-21 · ok",
        "two:20 · failed",
        "second read failure",
        "three:20 · running",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    assert!(!text.contains("Read ("), "{text}");
    transcript.progress(AgentEvent::ToolFinished {
        id: "three".into(),
        result: json!({"ok":true,"content":"three-body","lines_read":3,"truncated":true}),
    });
    let text = screen(&render(&mut transcript, 100, 30));
    for expected in [
        "one:20-21 · ok",
        "second read failure",
        "three:20-22 · ok · truncated",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    assert!(!text.contains("Read ("), "{text}");

    let mut transcript = Transcript::new();
    for id in ["first", "second"] {
        tool(&mut transcript, id, "read", json!({"path":id}));
        transcript.progress(AgentEvent::ToolOutput {
            id: id.into(),
            stream: OutputStream::Stdout,
            text: format!("{id}-partial-output"),
        });
    }
    assert!(screen(&render(&mut transcript, 100, 30)).contains("Read (2)"));
    transcript.fail();
    let text = screen(&render(&mut transcript, 100, 30));
    for expected in [
        "first · failed",
        "second · failed",
        "first-partial-output",
        "second-partial-output",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    assert!(!text.contains("Read ("), "{text}");
}

#[test]
fn read_summary_matches_reference_layout_colors_and_wrapping() {
    let mut transcript = Transcript::new();
    let files = [
        ("src/tools.rs", 184, 707),
        ("src/agent/plan_workflow.rs", 1, 315),
        ("src/events.rs", 1, 170),
        ("src/agent.rs", 1430, 1600),
    ];
    for (path, start, end) in files {
        tool(&mut transcript, path, "read", json!({"path":path}));
        transcript.progress(AgentEvent::ToolFinished {
                id: path.into(),
                result: json!({"ok":true,"source":path,"ranges":[{"start":start,"end":end}],"content":"retained body"}),
            });
    }
    let buffer = render(&mut transcript, 80, 10);
    let text = screen(&buffer);
    assert_eq!(
        text.lines().take(5).map(str::trim_end).collect::<Vec<_>>(),
        [
            "● Read (4)",
            "  ├─ src/tools.rs:184-707",
            "  ├─ src/agent/plan_workflow.rs:1-315",
            "  ├─ src/events.rs:1-170",
            "  └─ src/agent.rs:1430-1600",
        ]
    );
    assert_eq!(buffer[(0, 0)].fg, palette::READ_SUMMARY_TITLE);
    assert_eq!(buffer[(7, 0)].fg, palette::READ_SUMMARY_META);
    for row in 0..5 {
        for col in 0..80 {
            assert_eq!(buffer[(col, row)].bg, palette::READ_SUMMARY_BG);
            assert!(!buffer[(col, row)].modifier.contains(Modifier::BOLD));
        }
        if row > 0 {
            assert_eq!(buffer[(2, row)].fg, palette::READ_SUMMARY_META);
            assert_eq!(buffer[(5, row)].fg, palette::READ_SUMMARY_PATH);
        }
    }
    let narrow = screen(&render(&mut transcript, 20, 20));
    assert!(narrow.contains("  │  workflow.rs:1-3"), "{narrow}");
    assert!(narrow.contains("  └─ src/agent.rs:14"), "{narrow}");
    assert_eq!(transcript.entries.len(), 4);
    assert!(
        transcript
            .entries
            .iter()
            .all(|entry| entry.text == "retained body")
    );
}

#[test]
fn read_summary_groups_preserve_entries_results_and_full_replay() {
    let mut transcript = Transcript::new();
    for (id, offset) in [("one", 1), ("two", 20)] {
        tool(
            &mut transcript,
            id,
            "read",
            json!({"path":id,"offset":offset}),
        );
        transcript.progress(AgentEvent::ToolFinished {
            id: id.into(),
            result: json!({"ok":true,"content":format!("{id}-body\n"),"lines_read":1}),
        });
    }
    let text = screen(&render(&mut transcript, 100, 15));
    assert!(
        text.contains("Read (2)") && text.contains("one:1-1") && text.contains("two:20-20"),
        "{text}"
    );
    assert!(!text.contains("one-body") && !text.contains("two-body"));
    assert_eq!(transcript.entries.len(), 2);
    transcript.toggle_tool_output();
    assert!(!screen(&render(&mut transcript, 100, 15)).contains("one-body"));
    transcript.append("Assistant", "read group boundary");
    tool(&mut transcript, "failed", "read", json!({"path":"missing"}));
    transcript.progress(AgentEvent::ToolFinished {
        id: "failed".into(),
        result: json!({"ok":false,"error":"read failure is visible"}),
    });
    let snapshot = render(&mut transcript, 100, 20);
    assert!(screen(&snapshot).contains("read failure is visible"));
    let mut output = Vec::new();
    transcript
        .write_full_for_test(&mut output, &snapshot)
        .unwrap();
    let mut parser = vt100::Parser::new(80, 100, 0);
    parser.process(&output);
    let full = parser.screen().contents();
    assert!(
        full.contains("one-body") && full.contains("two-body"),
        "{full}"
    );
}

#[test]
fn tool_preview_counts_logical_lines_and_keeps_other_messages_complete() {
    for count in [3, 4] {
        let mut transcript = Transcript::with_read_preview(true);
        tool(&mut transcript, "read", "read", json!({"path":"file"}));
        transcript.progress(AgentEvent::ToolFinished { id:"read".into(), result:json!({"ok":true,"content":(0..count).map(|i|format!("line-{i:02}")).collect::<Vec<_>>().join("\n")}) });
        let text = entry_text(&mut transcript.entries[0], 80);
        assert_eq!(text.contains("to expand"), count > 3);
        assert!(text.contains("line-02") && !text.contains("line-03"));
    }
    let mut transcript = Transcript::with_read_preview(true);
    tool(&mut transcript, "long", "read", json!({}));
    transcript.entries[0].text = format!("{}\n{}", "中😀".repeat(9000), "tail\n");
    assert!(!entry_text(&mut transcript.entries[0], 10).contains("to expand"));
    let body = "unchanged\n".repeat(20);
    for label in ["You", "Assistant", "Help"] {
        transcript.append(label, &body);
    }
    transcript.progress(AgentEvent::ThinkingDelta { text: body.clone() });
    tool(
        &mut transcript,
        "ask",
        "ask",
        json!({"questions":[{"id":"q","question":body,"options":[]}]}),
    );
    let before: Vec<_> = transcript.entries[1..]
        .iter_mut()
        .map(|entry| entry_text(entry, 80))
        .collect();
    transcript.toggle_tool_output();
    transcript.toggle_tool_output();
    let after: Vec<_> = transcript.entries[1..]
        .iter_mut()
        .map(|entry| entry_text(entry, 80))
        .collect();
    assert_eq!(before, after);
    assert!(matches!(
        transcript.entries.last().unwrap().body,
        EntryBody::Ask(_)
    ));
}

#[test]
fn tool_toggle_anchors_history_and_exit_scrollback_expands_all_tools() {
    let mut transcript = Transcript::with_read_preview(true);
    for i in 0..3 {
        tool(
            &mut transcript,
            &format!("r{i}"),
            "read",
            json!({"path":format!("file{i}")}),
        );
        transcript.progress(AgentEvent::ToolFinished {
                id: format!("r{i}"),
                result: json!({"ok":true,"content":(0..40).map(|j| format!("file{i}-{j:02}")).collect::<Vec<_>>().join("\n")}),
            });
    }
    render(&mut transcript, 100, 10);
    let first_height = transcript.entries[0].layout.as_ref().unwrap().height;
    transcript.follow_bottom = false;
    transcript.offset = first_height + 1;
    assert!(screen(&render(&mut transcript, 100, 10)).contains("Read · file1"));
    transcript.toggle_tool_output();
    assert!(!transcript.follow_bottom);
    assert!(screen(&render(&mut transcript, 100, 10)).contains("Read · file1"));
    // 收起时锚点位于隐藏内容内，也必须留在同一卡片。
    transcript.offset += 30;
    transcript.toggle_tool_output();
    let start = transcript.entries[0].layout.as_ref().unwrap().height + 1;
    let end = start + transcript.entries[1].layout.as_ref().unwrap().height;
    assert!((start..=end).contains(&transcript.offset));
    let snapshot = render(&mut transcript, 100, 10);
    let mut output = Vec::new();
    transcript
        .write_full_for_test(&mut output, &snapshot)
        .unwrap();
    let mut parser = vt100::Parser::new(200, 100, 0);
    parser.process(&output);
    let text = parser.screen().contents();
    for i in 0..3 {
        assert!(text.contains(&format!("file{i}-39")), "{text}");
    }
    assert!(!text.contains("to expand"));
    assert!(!Transcript::with_read_preview(true).tools_expanded);
}

#[test]
fn ask_details_stay_in_messages_after_answering_and_resize() {
    let args = crate::interaction::AskArgs::parse(
        &json!({"questions":[{
            "id":"db", "question":"Database question?", "options":[{
                "label":"SQLite", "description":"description-marker\u{1b}",
                "preview":"**Preview**\n\nline-one\n\nline-two\n\npreview-tail"
            }, {"label":"Postgres", "description":"other-description"}]
        }]})
        .to_string(),
    )
    .unwrap();
    let mut block = AskBlock::new(args);
    let text = block
        .layout(80)
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        "Database question?",
        "󰐽 SQLite",
        "description-marker",
        "Preview",
        "preview-tail",
        "other-description",
    ] {
        assert!(text.contains(expected), "消息中应显示完整详情：{text}");
    }
    assert!(!text.contains('\u{1b}'));
    assert!(!text.contains("**Preview**"), "预览应按 Markdown 渲染");
    assert!(block.finish(&json!({"ok":true,"answers":[{
        "id":"db", "selected_options":["SQLite"], "custom_input":null
    }]})));
    for width in [32, 80] {
        let text = block
            .layout(width)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "󰐾 SQLite",
            "description-marker",
            "preview-tail",
            "󰐽 Postgres",
            "other-description",
        ] {
            assert!(text.contains(expected), "回答和缩放后仍应保留详情：{text}");
        }
    }
}
