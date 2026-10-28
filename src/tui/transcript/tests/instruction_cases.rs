use super::*;

#[test]
fn hashline_read_summary_and_skill_preview_remain_independent() {
    let mut transcript = Transcript::new();
    tool(&mut transcript, "normal", "read", json!({"path":"file"}));
    transcript.progress(AgentEvent::ToolFinished {id:"normal".into(),result:json!({"ok":true,"source":"file","ranges":[{"start":1,"end":2}],"lines_read":2,"content":"[file#A1B2]\n1:ordinary hidden\n2:tail\n"})});
    tool(
        &mut transcript,
        "skill",
        "read",
        json!({"path":"skill://rust"}),
    );
    transcript.progress(AgentEvent::ToolFinished {id:"skill".into(),result:json!({"ok":true,"source":"skill://rust","path":"/tmp/skills/rust/SKILL.md","content":"skill-first\nskill-second\nskill-third\nskill-fourth\n"})});
    tool(&mut transcript, "other", "read", json!({"path":"other"}));
    transcript.progress(AgentEvent::ToolFinished {
        id: "other".into(),
        result: json!({"ok":true,"source":"other","content":"other hidden"}),
    });
    let text = screen(&render(&mut transcript, 100, 30));
    assert!(text.contains("file:1-2"));
    assert!(!text.contains("ordinary hidden"));
    // 技能读取是独立 Read 卡片：完整短正文直接可见，不并入普通 read 摘要。
    assert!(
        text.contains("skill-first") && text.contains("skill-fourth"),
        "{text}"
    );
    assert!(text.contains("Read skill://rust"));
    assert!(text.contains("⟨Resolved path: /tmp/skills/rust/SKILL.md⟩"));
    assert_eq!(transcript.entries[1].kind, EntryKind::Read);
    assert!(!transcript.entries[1].hidden);
    assert!(!transcript.entries[2].hidden);
    transcript.toggle_tool_output();
    assert!(screen(&render(&mut transcript, 100, 30)).contains("skill-fourth"));
    let mut output = Vec::new();
    let snapshot = render(&mut transcript, 100, 30);
    transcript
        .write_full_for_test(&mut output, &snapshot)
        .unwrap();
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("[file#A1B2]"));
}

#[test]
fn instruction_cards_match_reference_layout_and_colors() {
    let content = "---\nname: rust\ndescription: >\n  Metadata description\n---\n\n# Rust guide\n\n**Important guidance**\nPlain text\nhidden line";
    for skill in [false, true] {
        let mut transcript = Transcript::new();
        if skill {
            tool(
                &mut transcript,
                "skill",
                "read",
                json!({"path":"skill://rust"}),
            );
            transcript.progress(AgentEvent::ToolFinished {
                id: "skill".into(),
                result: json!({"ok":true,"path":"/skills/rust/SKILL.md","content":content}),
            });
        } else {
            transcript.progress(AgentEvent::ProjectInstructionsLoaded {
                path: "/project/AGENTS.md".into(),
                content: content.into(),
            });
        }
        let buffer = render(&mut transcript, 80, 30);
        let text = screen(&buffer);
        assert!(
            text.contains(if skill {
                "· Read skill://rust"
            } else {
                "· Read AGENTS.md"
            }),
            "{text}"
        );
        assert!(
            !text.contains("# Rust") && !text.contains("**Important"),
            "{text}"
        );
        assert!(
            text.contains("name: rust") && text.contains("description: >"),
            "{text}"
        );
        assert!(text.contains("│   Metadata description"), "{text}");
        assert!(!text.contains("hidden line"), "{text}");
        assert_eq!(buffer[(0, 0)].fg, palette::READ_BORDER);
        assert_eq!(buffer[(3, 0)].fg, palette::ADDED);
        assert_eq!(buffer[(5, 0)].fg, palette::READ_TEXT);
        let cell = |needle: &str| {
            let row = text.lines().position(|line| line.contains(needle)).unwrap() as u16;
            &buffer[(2, row)]
        };
        // 元数据保持逐行原文；正文标题与强调使用截图中的金色层次。
        assert_eq!(cell("name: rust").fg, palette::READ_TEXT);
        let heading_row = text
            .lines()
            .position(|line| line.contains("Rust guide"))
            .unwrap() as u16;
        assert_eq!(buffer[(2, heading_row)].fg, palette::READ_ACCENT);
        assert!(
            buffer[(2, heading_row)]
                .modifier
                .contains(Modifier::UNDERLINED)
        );
        assert_eq!(cell("Important guidance").fg, palette::READ_ACCENT);
        assert_eq!(cell("Plain text").fg, palette::READ_TEXT);
        assert!(!cell("Plain text").modifier.contains(Modifier::DIM));
        assert_eq!(cell("more lines").fg, palette::DIM);
        assert_eq!(cell("Resolved path").fg, palette::DIM);
        assert_eq!(cell("Output").fg, palette::READ_TEXT);
        for row in 0..=text.lines().position(|line| line.contains('╰')).unwrap() as u16 {
            assert_eq!(buffer[(0, row)].bg, palette::READ_BG);
            assert_eq!(buffer[(79, row)].bg, palette::READ_BG);
        }
        transcript.toggle_tool_output();
        assert!(screen(&render(&mut transcript, 80, 30)).contains("hidden line"));
    }
}

#[test]
fn instruction_cards_preview_expand_and_stay_independent_of_read_groups() {
    let mut transcript = Transcript::new();
    transcript.progress(AgentEvent::ProjectInstructionsLoaded {
        path: "/tmp/proj/AGENTS.md".into(),
        content: (0..15)
            .map(|i| format!("agents-{i:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    });
    for id in ["a", "b"] {
        tool(
            &mut transcript,
            id,
            "read",
            json!({"path":format!("{id}.rs")}),
        );
        transcript.progress(AgentEvent::ToolFinished {
            id: id.into(),
            result: json!({"ok":true,"content":format!("{id}-body"),"lines_read":1}),
        });
    }
    tool(
        &mut transcript,
        "skill",
        "read",
        json!({"path":"skill://rust"}),
    );
    transcript.progress(AgentEvent::ToolFinished {
            id: "skill".into(),
            result: json!({"ok":true,"source":"skill://rust","path":"/tmp/skills/rust/SKILL.md","content":(0..12).map(|i|format!("skill-{i:02}")).collect::<Vec<_>>().join("\n")}),
        });
    for id in ["c", "d"] {
        tool(
            &mut transcript,
            id,
            "read",
            json!({"path":format!("{id}.rs")}),
        );
        transcript.progress(AgentEvent::ToolFinished {
            id: id.into(),
            result: json!({"ok":true,"content":format!("{id}-body"),"lines_read":1}),
        });
    }
    assert_eq!(
        transcript
            .entries
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            EntryKind::Read,
            EntryKind::Data,
            EntryKind::Data,
            EntryKind::Read,
            EntryKind::Data,
            EntryKind::Data,
        ]
    );
    let text = screen(&render(&mut transcript, 100, 60));
    assert!(text.contains("Read AGENTS.md"), "{text}");
    assert!(
        text.contains("agents-09") && !text.contains("agents-10"),
        "{text}"
    );
    assert!(text.contains("5 more lines (Ctrl + O to expand)"), "{text}");
    assert!(
        text.contains("⟨Resolved path: /tmp/proj/AGENTS.md⟩"),
        "{text}"
    );
    assert!(text.contains("Output"), "{text}");
    assert!(text.contains("Read skill://rust"), "{text}");
    assert!(
        text.contains("⟨Resolved path: /tmp/skills/rust/SKILL.md⟩"),
        "{text}"
    );
    assert!(
        text.contains("skill-09") && !text.contains("skill-10"),
        "{text}"
    );
    // 连续普通读取仍然折叠成组，指令卡片不并入组内。
    assert_eq!(text.matches("Read (2)").count(), 2, "{text}");
    assert!(!transcript.entries[0].hidden && !transcript.entries[3].hidden);
    // Ctrl + O 展开后两份完整正文可见，后续加载继承展开状态。
    transcript.toggle_tool_output();
    let text = screen(&render(&mut transcript, 100, 60));
    assert!(text.contains("agents-14"), "{text}");
    assert!(text.contains("skill-11"), "{text}");
    transcript.progress(AgentEvent::ProjectInstructionsLoaded {
        path: "/tmp/proj/AGENTS.md".into(),
        content: (0..12)
            .map(|i| format!("later-{i:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    });
    assert!(transcript.entries.last().unwrap().expanded);
    assert!(entry_text(transcript.entries.last_mut().unwrap(), 100).contains("later-11"));
}

#[test]
fn failed_and_fallback_skill_reads_stay_truthful() {
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
    assert_eq!(transcript.entries[0].kind, EntryKind::Read);
    assert_eq!(transcript.entries[0].status, EntryStatus::Failed);
    assert!(transcript.entries[0].footer.is_empty());
    let text = screen(&render(&mut transcript, 100, 20));
    assert!(text.contains("Read skill://missing"), "{text}");
    assert!(text.contains("skill not found: missing"), "{text}");
    assert!(!text.contains("Resolved path"), "{text}");

    // 参数无法识别时按完成结果来源兜底归类，并展示真实 resolved path。
    tool(&mut transcript, "late", "read", json!({"path":"unknown"}));
    transcript.progress(AgentEvent::ToolFinished {
            id: "late".into(),
            result: json!({"ok":true,"source":"skill://late","path":"/skills/late/SKILL.md","content":"late-body"}),
        });
    assert_eq!(transcript.entries[1].kind, EntryKind::Read);
    assert_eq!(transcript.entries[1].status, EntryStatus::Complete);
    let text = screen(&render(&mut transcript, 100, 40));
    assert!(text.contains("Read skill://late"), "{text}");
    assert!(
        text.contains("⟨Resolved path: /skills/late/SKILL.md⟩"),
        "{text}"
    );
    assert!(text.contains("late-body"), "{text}");
}

#[test]
fn instruction_cards_survive_narrow_widths_and_neutralize_controls() {
    let mut transcript = Transcript::new();
    transcript.progress(AgentEvent::ProjectInstructionsLoaded {
        path: "/tmp/proj/AGENTS.md".into(),
        content: "plain-line\n\u{1b}[31mred\u{1b}[0m\nlast\n".into(),
    });
    let text = screen(&render(&mut transcript, 8, 30));
    assert!(text.contains('╭') && text.contains('╰'), "{text}");
    assert!(!text.contains('\u{1b}'), "{text}");
    assert!(text.contains('\u{fffd}'), "{text}");
    // 折叠状态切换在窄宽度下同样安全。
    transcript.toggle_tool_output();
    let text = screen(&render(&mut transcript, 8, 30));
    assert!(text.contains('╭') && text.contains('╰'), "{text}");
    assert!(text.contains('\u{fffd}'), "{text}");
}

#[test]
fn instruction_cards_replay_tree_and_native_boundaries() {
    // 恢复时按记录顺序重建卡片正文，不读取当前磁盘内容。
    let mut transcript = Transcript::new();
    let mut input_history = Vec::new();
    let error = transcript.replay(
        vec![
            ReplayRecord::User("task".into()),
            ReplayRecord::Event(AgentEvent::ProjectInstructionsLoaded {
                path: "/old/AGENTS.md".into(),
                content: "recorded-body\n".into(),
            }),
            ReplayRecord::Event(AgentEvent::AssistantStarted),
            ReplayRecord::Event(AgentEvent::AssistantFinished {
                text: "done".into(),
            }),
        ],
        &mut input_history,
    );
    assert!(error.is_none());
    let text = screen(&render(&mut transcript, 100, 30));
    assert!(text.contains("Read AGENTS.md"), "{text}");
    assert!(text.contains("recorded-body"), "{text}");
    assert!(text.contains("⟨Resolved path: /old/AGENTS.md⟩"), "{text}");

    // 树预览完整展示卡片；极窄宽度仍不越界。
    let mut transcript = Transcript::new();
    let range = transcript.append_tree_node(
        vec![ReplayRecord::Event(AgentEvent::ProjectInstructionsLoaded {
            path: "/tree/AGENTS.md".into(),
            content: "tree\n".into(),
        })],
        "",
    );
    assert_eq!(range, 0..1);
    let text = entry_text(&mut transcript.entries[0], 100);
    assert!(text.contains("tree"), "{text}");
    let blocks = [TreeBlock {
        entries: range,
        label: Line::raw("node"),
    }];
    let wide = tree_render(&mut transcript, &blocks, &[0], Some(0), true, 60, 12);
    let wide_text = screen(&wide);
    assert!(wide_text.contains("tree"), "{wide_text}");
    assert!(wide_text.contains("Read AGENTS.md"), "{wide_text}");
    let narrow = tree_render(&mut transcript, &blocks, &[0], Some(0), true, 2, 6);
    assert!(screen(&narrow).contains('t'));

    // 卡片进入原生历史批次一次，不重复输出。
    let mut transcript = Transcript::new();
    transcript.append("You", "run");
    transcript.progress(AgentEvent::ProjectInstructionsLoaded {
        path: "/native/AGENTS.md".into(),
        content: "native-body\n".into(),
    });
    transcript.append("Assistant", &"tail-line\n\n".repeat(40));
    let output = retire_native(&mut transcript, 60, 30, false);
    assert!(output.contains("Read AGENTS.md"), "{output}");
    assert_eq!(output.matches("native-body").count(), 1, "{output}");
    assert!(
        retire_native(&mut transcript, 60, 30, false)
            .trim()
            .is_empty()
    );
}
