use super::*;

#[test]
fn instruction_reads_share_normal_groups_and_expansion() {
    let mut transcript = Transcript::new();
    transcript.progress(AgentEvent::ProjectInstructionsLoaded {
        path: "/project/AGENTS.md".into(),
        content: "# Rules\n**literal guidance**\nlast-rule".into(),
    });
    for (id, path) in [("file", "file.rs"), ("skill", "skill://rust")] {
        tool(&mut transcript, id, "read", json!({"path":path}));
        transcript.progress(AgentEvent::ToolFinished {
            id: id.into(),
            result: json!({"ok":true,"source":path,"path":"/skills/rust/SKILL.md","content":format!("{id}-body\n{id}-tail")}),
        });
    }
    let text = screen(&render(&mut transcript, 100, 30));
    assert!(text.contains("Read (3)"), "{text}");
    for path in ["/project/AGENTS.md", "file.rs", "skill://rust"] {
        assert!(text.contains(path), "{text}");
    }
    assert!(
        !text.contains("Rules") && !text.contains("skill-body"),
        "{text}"
    );
    assert!(!text.contains("Resolved path"), "{text}");
    assert!(
        transcript
            .entries
            .iter()
            .all(|entry| entry.kind == EntryKind::Data)
    );
    transcript.toggle_tool_output();
    let text = screen(&render(&mut transcript, 100, 40));
    for body in ["# Rules", "**literal guidance**", "file-tail", "skill-tail"] {
        assert!(text.contains(body), "{text}");
    }
    assert!(!text.contains("Read (3)"), "{text}");
}

#[test]
fn instruction_reads_obey_three_line_preview_setting() {
    let mut transcript = Transcript::new();
    transcript.display.read_preview = true;
    let content = "first\nsecond\nthird\nfourth\nfifth";
    transcript.progress(AgentEvent::ProjectInstructionsLoaded {
        path: "/project/AGENTS.md".into(),
        content: content.into(),
    });
    tool(
        &mut transcript,
        "skill",
        "read",
        json!({"path":"skill://rust"}),
    );
    transcript.progress(AgentEvent::ToolFinished {
        id: "skill".into(),
        result: json!({"ok":true,"content":content}),
    });
    let text = screen(&render(&mut transcript, 100, 30));
    assert_eq!(text.matches("third").count(), 2, "{text}");
    assert!(!text.contains("fourth"), "{text}");
    assert_eq!(
        text.matches("2 more lines (Ctrl + O to expand)").count(),
        2,
        "{text}"
    );
    transcript.toggle_tool_output();
    assert_eq!(
        screen(&render(&mut transcript, 100, 40))
            .matches("fifth")
            .count(),
        2
    );
}

#[test]
fn failed_skill_reads_keep_errors_and_normal_read_style() {
    let mut transcript = Transcript::new();
    tool(
        &mut transcript,
        "skill",
        "read",
        json!({"path":"skill://missing"}),
    );
    transcript.progress(AgentEvent::ToolFinished {
        id: "skill".into(),
        result: json!({"ok":false,"error":"skill not found: missing"}),
    });
    let text = screen(&render(&mut transcript, 100, 20));
    assert!(
        text.contains("skill://missing") && text.contains("skill not found: missing"),
        "{text}"
    );
    assert_eq!(transcript.entries[0].status, EntryStatus::Failed);
    assert_eq!(transcript.entries[0].kind, EntryKind::Data);
    assert!(!text.contains("Resolved path"), "{text}");
}

#[test]
fn instruction_reads_neutralize_controls_at_narrow_widths() {
    let mut transcript = Transcript::new();
    transcript.progress(AgentEvent::ProjectInstructionsLoaded {
        path: "/project/AGENTS.md".into(),
        content: "plain\n\u{1b}[31mred\u{1b}[0m\nlast".into(),
    });
    transcript.toggle_tool_output();
    let text = screen(&render(&mut transcript, 8, 40));
    assert!(!text.contains('\u{1b}'), "{text}");
    assert!(text.contains('\u{fffd}'), "{text}");
}

#[test]
fn instruction_reads_replay_tree_and_native_boundaries() {
    let event = || AgentEvent::ProjectInstructionsLoaded {
        path: "/old/AGENTS.md".into(),
        content: "recorded-body\n".into(),
    };
    let mut transcript = Transcript::new();
    assert!(
        transcript
            .replay(vec![ReplayRecord::Event(event())])
            .is_none()
    );
    let text = screen(&render(&mut transcript, 100, 30));
    assert!(
        text.contains("/old/AGENTS.md") && !text.contains("recorded-body"),
        "{text}"
    );
    transcript.toggle_tool_output();
    assert!(screen(&render(&mut transcript, 100, 30)).contains("recorded-body"));

    let mut transcript = Transcript::new();
    let range = transcript.append_tree_node(vec![ReplayRecord::Event(event())], "");
    assert_eq!(range, 0..1);
    let blocks = [TreeBlock {
        entries: range,
        label: Line::raw("node"),
    }];
    let text = screen(&tree_render(
        &mut transcript,
        &blocks,
        &[0],
        Some(0),
        true,
        60,
        12,
    ));
    assert!(text.contains("recorded-body"), "{text}");

    let mut transcript = Transcript::new();
    transcript.append("You", "run");
    transcript.progress(event());
    transcript.append("Assistant", &"tail-line\n\n".repeat(40));
    let output = retire_native(&mut transcript, 60, 30, false);
    assert_eq!(output.matches("/old/AGENTS.md").count(), 1, "{output}");
    assert!(!output.contains("recorded-body"), "{output}");
    assert!(
        retire_native(&mut transcript, 60, 30, false)
            .trim()
            .is_empty()
    );
}
