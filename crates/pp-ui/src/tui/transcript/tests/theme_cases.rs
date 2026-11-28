use super::*;

#[test]
fn streaming_backgrounds_roles_and_thinking_styles() {
    for kind in [EntryKind::Rg, EntryKind::Data] {
        let mut transcript = Transcript::new();
        let tool = kind == EntryKind::Rg;
        let mut entry = TranscriptEntry::new(
            kind,
            "title".into(),
            "  body 😀\n\n    code".into(),
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
        assert!(screen(&buffer).contains("  body 😀"));
        assert!(screen(&buffer).contains("    code"));
    }

    {
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

    {
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
        assert!(icons.insert(icon), "tool icons must be distinct: {name}");
        assert!(
            icon.chars().next().is_some_and(|glyph| {
                ('\u{e000}'..='\u{f8ff}').contains(&glyph)
                    || ('\u{f0000}'..='\u{ffffd}').contains(&glyph)
            }),
            "the icon must use a Nerd Font private use area glyph: {name}"
        );
        assert_eq!(
            Span::raw(icon).width(),
            1,
            "the icon must take one layout column: {name}"
        );
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
                // Narrow widths may clip the card title; check the layout only when the icon is actually visible.
                let Some(row) = (0..buffer.area.height).find_map(|row| {
                    (0..buffer.area.width)
                        .find(|&column| buffer[(column, row)].symbol() == display_icon)
                        .map(|column| (column, row))
                }) else {
                    continue;
                };
                // Exactly one ASCII space is kept between the icon and the title.
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
fn thinking_turns_markdown_highlights_and_streaming_deltas() {
    {
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

    {
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
        // Plain text keeps the gray tone plus the dark assistant background.
        let plain = cell_of("plain");
        assert_eq!(buffer[plain].fg, muted_text());
        assert_eq!(buffer[plain].bg, assistant_background());
        // Bold adds the BOLD modifier on top of the purple semantic highlight.
        let bold = cell_of("bold");
        assert_eq!(buffer[bold].fg, palette::md_strong());
        assert!(buffer[bold].modifier.contains(Modifier::BOLD));
        // Inline code uses its own green semantic highlight.
        let code = cell_of("code");
        assert_eq!(buffer[code].fg, palette::md_code());
        assert_eq!(buffer[code].bg, assistant_background());
        // A fenced code block is highlighted by syntect semantics, with `let` in the keyword color. The line-level gray
        // tone of the thinking body is overridden by the semantic span, matching the bold/inline-code convention.
        let block = cell_of("let x = 1;");
        assert_eq!(buffer[block].fg, palette::syntax_keyword());
        // The table renders real separators instead of the raw pipe characters.
        assert!(text.contains('│'));
        assert!(text.contains("─┼─"));
        assert!(!text.contains("**"));
        assert!(!text.contains('`'));
        assert!(!text.contains("---"));
    }

    {
        let mut transcript = Transcript::new();
        transcript.progress(AgentEvent::AssistantStarted);
        transcript.progress(AgentEvent::ThinkingDelta {
            text: "**bo".into(),
        });
        // Trigger the layout cache first, then complete the marker to verify that the new chunk invalidates the old parse.
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
}
