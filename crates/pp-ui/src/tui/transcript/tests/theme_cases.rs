use super::*;

#[test]
fn streaming_category_colors_and_statuses_preserve_default_body() {
    for kind in [EntryKind::Rg, EntryKind::Data] {
        let mut transcript = Transcript::new();
        let tool = kind == EntryKind::Rg;
        let mut entry = TranscriptEntry::new(
            kind,
            "title".into(),
            "  body 中😀\n\n    code".into(),
            tool.then(|| "tool".into()),
        );
        entry.status = EntryStatus::Failed;
        transcript.entries.push(entry);
        let buffer = render(&mut transcript, 60, 8);
        assert_eq!(buffer[(0, 0)].symbol(), "│");
        assert_eq!(
            buffer[(0, 0)].fg,
            if tool {
                palette::tool_border()
            } else {
                kind.color()
            }
        );
        assert_eq!(
            buffer[(2, 0)].fg,
            if tool {
                palette::activity()
            } else {
                kind.color()
            }
        );
        assert!(buffer[(2, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(4, 1)].fg, palette::text());
        if tool {
            assert!(buffer[(4, 1)].modifier.contains(Modifier::DIM));
        }
        assert!(screen(&buffer).contains("  body 中"));
        assert!(screen(&buffer).contains("    code"));
    }
}

#[test]
fn conversation_entries_use_backgrounds_without_role_labels() {
    let mut transcript = Transcript::new();
    transcript.append("You", "user input");
    transcript.append("Assistant", "assistant output");

    let buffer = render(&mut transcript, 60, 8);
    let text = screen(&buffer);
    assert_eq!(transcript.entries[0].kind, EntryKind::User);
    assert_eq!(transcript.entries[1].kind, EntryKind::Assistant);
    assert_eq!(buffer[(0, 0)].symbol(), " ");
    assert_eq!(buffer[(0, 0)].bg, user_background());
    assert_eq!(buffer[(2, 0)].bg, user_background());
    assert_eq!(buffer[(2, 1)].symbol(), "u");
    assert_eq!(buffer[(2, 1)].bg, user_background());
    assert_eq!(buffer[(0, 2)].symbol(), " ");
    assert_eq!(buffer[(0, 2)].bg, user_background());
    assert_eq!(buffer[(0, 4)].bg, assistant_background());
    assert_eq!(buffer[(2, 4)].bg, assistant_background());
    assert_ne!(user_background(), assistant_background());
    assert!(text.contains("user input"));
    assert!(text.contains("assistant output"));
    assert!(!text.contains("You"));
    assert!(!text.contains("Assistant"));
}

#[test]
fn thinking_and_assistant_share_background_but_not_foreground() {
    let mut transcript = Transcript::new();
    transcript.append("Assistant", "answer");
    transcript.progress(AgentEvent::ThinkingDelta {
        text: "reasoning".into(),
    });

    let buffer = render(&mut transcript, 60, 4);
    assert_eq!(buffer[(2, 0)].fg, palette::md_text());
    assert_eq!(buffer[(2, 0)].bg, assistant_background());
    assert_eq!(buffer[(2, 2)].fg, muted_text());
    assert_eq!(buffer[(2, 2)].bg, assistant_background());
}

#[test]
fn tool_icons_use_nerd_font_private_use_glyphs_across_card_styles() {
    let names = [
        "read",
        "edit",
        "write",
        "bash_task",
        "rg",
        "find",
        "ask",
        "plan_search",
        "plan_find",
        "plan_submit",
        "mcp__fixture__tool",
        "unknown_tool",
    ];
    let mut icons = std::collections::HashSet::new();
    for name in names {
        let icon = palette::tool_icon(name);
        assert!(icons.insert(icon), "工具图标应独立：{name}");
        assert!(
            icon.chars().next().is_some_and(|glyph| {
                ('\u{e000}'..='\u{f8ff}').contains(&glyph)
                    || ('\u{f0000}'..='\u{ffffd}').contains(&glyph)
            }),
            "图标应使用 Nerd Font 私用区字形：{name}"
        );
        assert_eq!(Span::raw(icon).width(), 1, "图标布局宽度应为一列：{name}");
        let mut transcript = Transcript::new();
        tool(
            &mut transcript,
            "call",
            name,
            json!({"path":"file", "command":"true", "name":"rust"}),
        );
        let prefix = format!("{icon} ");
        let title = transcript.entries[0].title.clone();
        assert!(title.starts_with(&prefix));
        assert_eq!(Span::raw(&prefix).width(), 2);
        for result in [None, Some(json!({"ok":false,"error":"fixture error"}))] {
            if let Some(result) = result {
                transcript.progress(AgentEvent::ToolFinished {
                    id: "call".into(),
                    result,
                });
            }
            for width in [80, 20, 80] {
                let buffer = render(&mut transcript, width, 20);
                let display_icon = if name == "read" {
                    if transcript.entries[0].status == EntryStatus::Running {
                        "\u{f1456}"
                    } else {
                        "\u{f00d}"
                    }
                } else {
                    icon
                };
                // 窄宽度允许卡片标题被裁切；仅在图标实际可见时校验其布局。
                let Some(row) = (0..buffer.area.height).find_map(|row| {
                    (0..buffer.area.width)
                        .find(|&column| buffer[(column, row)].symbol() == display_icon)
                        .map(|column| (column, row))
                }) else {
                    continue;
                };
                // 图标与标题之间仅保留一列 ASCII 空格。
                assert_eq!(buffer[(row.0 + 1, row.1)].symbol(), " ");
                if width == 80 {
                    assert_eq!(
                        buffer[(row.0 + Span::raw(display_icon).width() as u16 + 1, row.1)]
                            .symbol(),
                        if name == "read" {
                            "R".into()
                        } else if name == "rg" {
                            "G".into()
                        } else if name == "edit" {
                            "E".into()
                        } else {
                            title[prefix.len()..].chars().next().unwrap().to_string()
                        }
                    );
                }
            }
        }
    }
}

#[test]
fn thinking_turns_stay_separate_from_answers_and_each_other() {
    let mut transcript = Transcript::new();
    for _ in 0..2 {
        transcript.progress(AgentEvent::AssistantStarted);
        transcript.progress(AgentEvent::ThinkingDelta {
            text: "think".into(),
        });
        transcript.progress(AgentEvent::ThinkingDelta {
            text: " more".into(),
        });
        transcript.progress(AgentEvent::AssistantFinished {
            text: String::new(),
        });
    }
    assert_eq!(transcript.entries.len(), 2);
    assert_eq!(transcript.entries[0].text, "think more");
    transcript.progress(AgentEvent::AssistantStarted);
    transcript.progress(AgentEvent::ThinkingDelta {
        text: "partial thinking".into(),
    });
    delta(&mut transcript, "answer", false);
    transcript.fail();
    let text = screen(&render(&mut transcript, 80, 20));
    assert!(text.contains("partial thinking"));
    assert!(text.contains("answer"));
    assert!(!text.contains("stream failed"));
}

#[test]
fn thinking_renders_markdown_with_muted_base_and_semantic_highlights() {
    let mut transcript = Transcript::new();
    transcript.progress(AgentEvent::AssistantStarted);
    transcript.progress(AgentEvent::ThinkingDelta {
        text: "plain intro\n\n**bold** and `code`\n\n```rust\nlet x = 1;\n```\n\n| left | right |\n| --- | --- |\n| a | b |\n".into(),
    });
    let buffer = render(&mut transcript, 80, 30);
    let text = screen(&buffer);
    let cell_of = |needle: &str| -> (u16, u16) {
        for (row, line) in text.lines().enumerate() {
            if let Some(column) = line.find(needle) {
                return (column as u16, row as u16);
            }
        }
        panic!("missing {needle} in:\n{text}");
    };
    // 普通文本保持灰色基调 + 助手暗色背景。
    let plain = cell_of("plain");
    assert_eq!(buffer[plain].fg, muted_text());
    assert_eq!(buffer[plain].bg, assistant_background());
    // 粗体叠加 BOLD modifier 与紫色语义高亮。
    let bold = cell_of("bold");
    assert_eq!(buffer[bold].fg, palette::md_strong());
    assert!(buffer[bold].modifier.contains(Modifier::BOLD));
    // 行内代码使用独立的绿色语义高亮。
    let code = cell_of("code");
    assert_eq!(buffer[code].fg, palette::md_code());
    assert_eq!(buffer[code].bg, assistant_background());
    // 围栏代码块按 syntect 语义高亮，`let` 为 keyword 色。思考正文的行级灰
    // 基调被语义 span 覆盖，与粗体/行内代码的约定一致。
    let block = cell_of("let x = 1;");
    assert_eq!(buffer[block].fg, palette::syntax_keyword());
    // 表格渲染出真实分隔线而非原始管道符。
    assert!(text.contains('│'));
    assert!(text.contains("─┼─"));
    assert!(!text.contains("**"));
    assert!(!text.contains('`'));
    assert!(!text.contains("---"));
}

#[test]
fn thinking_markdown_reparses_across_streaming_deltas() {
    let mut transcript = Transcript::new();
    transcript.progress(AgentEvent::AssistantStarted);
    transcript.progress(AgentEvent::ThinkingDelta {
        text: "**bo".into(),
    });
    // 先触发布局缓存，再补齐标记，验证新片段会使旧解析结果失效。
    assert!(screen(&render(&mut transcript, 80, 10)).contains("**bo"));
    transcript.progress(AgentEvent::ThinkingDelta {
        text: "ld**".into(),
    });
    let text = screen(&render(&mut transcript, 80, 10));
    assert!(!text.contains('*'), "{text}");
    transcript.progress(AgentEvent::ThinkingDelta {
        text: " tail".into(),
    });
    let buffer = render(&mut transcript, 80, 10);
    let text = screen(&buffer);
    let (row, line) = text
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("bold"))
        .expect("bold rendered");
    let column = line.find("bold").expect("bold rendered") as u16;
    let row = row as u16;
    assert_eq!(buffer[(column, row)].fg, palette::md_strong());
    assert!(buffer[(column, row)].modifier.contains(Modifier::BOLD));
    assert!(text.contains("tail"));
}
