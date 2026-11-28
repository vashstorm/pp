use super::*;
use crate::tui::themes;
use pp_config::config::{IconMode, Theme, ToolStyle};
use pp_core::events::{DiffHunk, DiffLine};

fn build_sample_transcript() -> Transcript {
    let mut t = Transcript::new();
    // 1. Read group (2 files)
    for (id, path, end) in [
        ("r1", "crates/pp-config/src/config/theme.rs", 42),
        ("r2", "crates/pp-ui/src/tui/themes.rs", 120),
    ] {
        tool(
            &mut t,
            id,
            "read",
            json!({"path": path, "offset": 1, "limit": end}),
        );
        t.progress(AgentEvent::ToolFinished {
            id: id.into(),
            result: json!({"ok": true, "source": path, "lines_read": end, "content": "ok"}),
        });
    }
    // 2. Bash finished
    tool(
        &mut t,
        "b1",
        "bash",
        json!({"command": "cargo nextest run -p pp-ui"}),
    );
    t.progress(AgentEvent::ToolOutput {
        id: "b1".into(),
        stream: OutputStream::Stdout,
        text: "running 12 tests\ntest result: ok. 12 passed\n".into(),
    });
    t.progress(AgentEvent::ToolFinished {
        id: "b1".into(),
        result: json!({"ok": true, "exit_code": 0, "stdout": "running 12 tests\ntest result: ok. 12 passed\n", "stderr": ""}),
    });
    // 3. Grep
    tool(
        &mut t,
        "g1",
        "rg",
        json!({"pattern": "Theme::ALL", "path": "crates"}),
    );
    t.progress(AgentEvent::ToolFinished {
        id: "g1".into(),
        result: json!({
            "ok": true,
            "content": "# crates/pp-config/src/config/theme.rs\n*60: pub const ALL: [Self; 20]",
            "details": {"outputMode": "content", "matchCount": 1, "fileCount": 1, "fileMatches": [{"path": "crates/pp-config/src/config/theme.rs"}]}
        }),
    });
    // 4. Edit
    tool(&mut t, "e1", "edit", json!({"path": "src/tui/themes.rs"}));
    t.progress(AgentEvent::FileChanged {
        id: "e1".into(),
        path: "src/tui/themes.rs".into(),
        diff: FileDiff::Text {
            created: false,
            deleted: false,
            added: 1,
            removed: 1,
            hunks: vec![DiffHunk {
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Removed,
                        old_line: Some(12),
                        new_line: None,
                        text: "const THEME: &str = \"old\";\n".into(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Added,
                        old_line: None,
                        new_line: Some(12),
                        text: "let theme = \"tokyonight\";\n".into(),
                    },
                ],
            }],
        },
    });
    t.progress(AgentEvent::ToolFinished {
        id: "e1".into(),
        result: json!({"ok": true}),
    });
    // 5. MCP
    tool(
        &mut t,
        "m1",
        "mcp__memory__search",
        json!({"query": "themes"}),
    );
    t.progress(AgentEvent::ToolFinished {
        id: "m1".into(),
        result: json!({"ok": true, "result": {"content": [{"type": "text", "text": "matched 3 nodes in pp-ui"}]}}),
    });
    t
}

fn build_all_tools_transcript() -> Transcript {
    let mut t = build_sample_transcript();
    // Eval Python
    tool(
        &mut t,
        "ev1",
        "eval",
        json!({"language": "py", "title": "Inspect palette", "code": "print(len(themes))"}),
    );
    t.progress(AgentEvent::ToolFinished {
        id: "ev1".into(),
        result: json!({"ok": true, "language": "py", "execution_count": 1, "stdout": "20\n"}),
    });
    // Glob
    tool(
        &mut t,
        "gl1",
        "glob",
        json!({"pattern": "*.rs", "path": "crates/pp-ui"}),
    );
    t.progress(AgentEvent::ToolFinished {
        id: "gl1".into(),
        result: json!({"ok": true, "content": "src/tui/themes.rs\nsrc/tui/settings.rs"}),
    });
    // Web Search
    tool(
        &mut t,
        "ws1",
        "web_search",
        json!({"query": "ratatui buffer"}),
    );
    t.progress(AgentEvent::ToolFinished {
        id: "ws1".into(),
        result: json!({"ok": true, "provider_label": "Tavily", "details": {"source_count": 2}, "content": "[1] Ratatui\nhttps://ratatui.rs"}),
    });
    // Write
    tool(
        &mut t,
        "w1",
        "write",
        json!({"path": "out.txt", "content": "hello\nworld"}),
    );
    t.progress(AgentEvent::ToolFinished {
        id: "w1".into(),
        result: json!({"ok": true, "bytes_written": 11}),
    });
    // Wait
    tool(&mut t, "wt1", "wait", json!({}));
    t.progress(AgentEvent::ToolFinished {
        id: "wt1".into(),
        result: json!({"ok": true, "jobs": [{"id": "j1", "type": "bash", "status": "completed", "duration_ms": 1200, "label": "cargo test"}]}),
    });
    // Failed Bash
    tool(&mut t, "b2", "bash", json!({"command": "false"}));
    t.progress(AgentEvent::ToolFinished {
        id: "b2".into(),
        result: json!({"ok": false, "exit_code": 1, "stdout": "", "stderr": "boom\n"}),
    });
    t
}

#[test]
fn style_matrix_renders_across_widths_and_icon_modes_without_pua_in_ascii() {
    for style in ToolStyle::ALL {
        for icons in IconMode::ALL {
            for theme in [Theme::Default, Theme::SolarizedLight] {
                themes::set_active_appearance(theme, icons, style);
                let mut t = build_all_tools_transcript();
                for width in [8, 24, 40, 80, 120] {
                    let buffer = render(&mut t, width, 90);
                    let text = screen(&buffer);
                    assert!(
                        !text.trim().is_empty(),
                        "empty render for {style:?} {width}"
                    );
                    if width >= 40 {
                        assert!(text.contains("Grep"), "{style:?} {width}:\n{text}");
                    }
                    if icons == IconMode::Ascii {
                        assert!(
                            !text
                                .chars()
                                .any(|ch| ('\u{e000}'..='\u{f8ff}').contains(&ch)
                                    || ('\u{f0000}'..='\u{ffffd}').contains(&ch)),
                            "ASCII mode leaked PUA glyph in {style:?} width={width}:\n{text}"
                        );
                    }
                    if style == ToolStyle::Slab && width >= 40 {
                        let expected_bg = card::tone::slab_body_bg(themes::palette());
                        // The Slab tool card body row background must not fall back to Reset
                        let edit_row = text
                            .lines()
                            .position(|line| line.contains("tokyonight"))
                            .unwrap() as u16;
                        assert_eq!(buffer[(0, edit_row)].bg, expected_bg);
                        assert_eq!(buffer[(width - 1, edit_row)].bg, expected_bg);
                    }
                }
            }
        }
    }
    themes::set_active_appearance(Theme::Default, IconMode::Nerd, ToolStyle::Classic);
}

#[test]
fn new_styles_align_body_columns_and_right_status_consistently() {
    let cases = [
        (ToolStyle::Rail, 3u16, Some(78u16)),
        (ToolStyle::Slab, 3, Some(78)),
        (ToolStyle::Badge, 3, Some(78)),
        (ToolStyle::Hanging, 4, None),
        (ToolStyle::Ledger, 8, Some(78)),
    ];
    for (style, expected_body_col, expected_status_col) in cases {
        themes::set_active_appearance(Theme::Default, IconMode::Nerd, style);
        let mut t = build_sample_transcript();
        let buffer = render(&mut t, 80, 50);
        let text = screen(&buffer);

        // Verify that Bash / Grep / Edit / MCP bodies start their first character in the same column
        let bash_row = text
            .lines()
            .position(|l| l.contains("running 12 tests"))
            .unwrap() as u16;
        let mcp_row = text
            .lines()
            .position(|l| l.contains("matched 3 nodes"))
            .unwrap() as u16;
        let bash_col = text
            .lines()
            .nth(bash_row as usize)
            .unwrap()
            .find("running 12 tests")
            .unwrap();
        let mcp_col = text
            .lines()
            .nth(mcp_row as usize)
            .unwrap()
            .find("matched 3 nodes")
            .unwrap();
        assert_eq!(
            Line::raw(&text.lines().nth(bash_row as usize).unwrap()[..bash_col]).width() as u16,
            expected_body_col,
            "bash body col mismatch for {style:?}:\n{text}"
        );
        assert_eq!(
            Line::raw(&text.lines().nth(mcp_row as usize).unwrap()[..mcp_col]).width() as u16,
            expected_body_col,
            "mcp body col mismatch for {style:?}:\n{text}"
        );

        if let Some(status_col) = expected_status_col {
            for needle in ["2 files", "Bash", "Theme::ALL", "Edit", "memory search"] {
                let row = text
                    .lines()
                    .position(|l| {
                        l.to_ascii_lowercase()
                            .contains(&needle.to_ascii_lowercase())
                    })
                    .unwrap_or_else(|| panic!("missing {needle:?} in {style:?}:\n{text}"))
                    as u16;
                assert_eq!(
                    buffer[(status_col, row)].symbol(),
                    palette::done_icon(IconMode::Nerd),
                    "right status column misaligned on {needle} for {style:?} row={row}:\n{text}"
                );
            }
        }
    }
    themes::set_active_appearance(Theme::Default, IconMode::Nerd, ToolStyle::Classic);
}

#[test]
fn badge_foreground_meets_wcag_contrast_across_all_20_themes() {
    let tools = [
        "read",
        "rg",
        "glob",
        "edit",
        "write",
        "bash",
        "eval",
        "ask",
        "todo",
        "mcp__srv__run",
    ];
    for theme in Theme::ALL {
        themes::set_active_appearance(theme, IconMode::Nerd, ToolStyle::Badge);
        for name in tools {
            let accent = palette::tool_accent(name);
            let fg = card::tone::badge_fg(accent);
            let ratio = card::tone::contrast_ratio(accent, fg).unwrap();
            assert!(
                ratio >= 4.5,
                "low contrast ({ratio:.2}) on {theme:?} tool={name}"
            );
        }
        let removed = palette::removed();
        let fg = card::tone::badge_fg(removed);
        assert!(card::tone::contrast_ratio(removed, fg).unwrap() >= 4.5);
    }
    themes::set_active_appearance(Theme::Default, IconMode::Nerd, ToolStyle::Classic);
}

#[test]
fn native_scrollback_preserves_committed_style_when_tool_style_changes() {
    themes::set_active_appearance(Theme::Default, IconMode::Nerd, ToolStyle::Classic);
    let mut t = Transcript::new();
    tool(&mut t, "b1", "bash", json!({"command": "echo hi"}));
    t.progress(AgentEvent::ToolFinished {
        id: "b1".into(),
        result: json!({"ok": true, "exit_code": 0, "stdout": "hi\n", "stderr": ""}),
    });
    t.append("Assistant", "following line to push b1 into scrollback");
    let committed = retire_native(&mut t, 70, 2, false);
    assert!(committed.contains("╭─ Bash"), "{committed}");

    themes::set_active_appearance(Theme::Default, IconMode::Nerd, ToolStyle::Rail);
    t.invalidate_theme();
    tool(&mut t, "b2", "bash", json!({"command": "echo next"}));
    t.progress(AgentEvent::ToolFinished {
        id: "b2".into(),
        result: json!({"ok": true, "exit_code": 0, "stdout": "next\n", "stderr": ""}),
    });
    let mut terminal = Terminal::new(TestBackend::new(70, 20)).unwrap();
    terminal.draw(|frame| t.draw(frame, frame.area())).unwrap();
    let tail = screen(terminal.backend().buffer());
    // The new card uses the Rail style while committed rows keep their Classic snapshot
    assert!(committed.contains("╭─ Bash"));
    assert!(tail.contains("● Bash") || tail.contains("╰─"), "{tail}");
    themes::set_active_appearance(Theme::Default, IconMode::Nerd, ToolStyle::Classic);
}

#[test]
fn classic2_uses_per_tool_color_themes_and_unified_icon_names() {
    themes::set_active_appearance(Theme::Default, IconMode::Nerd, ToolStyle::Classic2);

    let tool_names = [
        "read",
        "edit",
        "write",
        "bash",
        "bash_task",
        "eval",
        "rg",
        "glob",
        "web_search",
        "ask",
        "task",
        "todo",
        "wait",
        "lsp",
        "plan_search",
        "plan_find",
        "plan_submit",
        "mcp__srv__run",
    ];
    let mut title_colors = std::collections::HashSet::new();
    let mut border_colors = std::collections::HashSet::new();
    let mut bg_colors = std::collections::HashSet::new();
    for name in tool_names {
        let theme = palette::classic2_tool_theme(name);
        assert!(
            title_colors.insert(format!("{:?}", theme.title)),
            "classic2 title color should be distinct for {name}"
        );
        assert!(
            border_colors.insert(format!("{:?}", theme.border)),
            "classic2 border color should be distinct for {name}"
        );
        assert!(
            bg_colors.insert(format!("{:?}", theme.bg)),
            "classic2 background color should be distinct for {name}"
        );
    }

    let mut t = Transcript::new();
    tool(&mut t, "r1", "read", json!({"path": "src/lib.rs"}));
    tool(&mut t, "b1", "bash", json!({"command": "echo hi"}));
    tool(
        &mut t,
        "l1",
        "lsp",
        json!({"action": "diagnostics", "file": "src/lib.rs"}),
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
    terminal.draw(|frame| t.draw(frame, frame.area())).unwrap();
    let out = screen(terminal.backend().buffer());

    assert!(
        out.contains(&format!("{} Read", palette::tool_icon("read"))),
        "{out}"
    );
    assert!(
        out.contains(&format!("{} Bash", palette::tool_icon("bash"))),
        "{out}"
    );
    assert!(
        out.contains(&format!("{} LSP", palette::tool_icon("lsp"))),
        "{out}"
    );

    themes::set_active_appearance(Theme::Default, IconMode::Nerd, ToolStyle::Classic);
}
