use super::*;

#[test]
fn rg_reference_layout_output_modes_groups_and_replay() {
    {
        let path = "/workspace/pi-coding-agent/src/tools/todo.ts";
        let arguments =
            json!({"pattern":"function applyEntry|function formatSummary", "path":path});
        let result = json!({"ok":true,
                "content":"[todo.ts#abc123]\n*399: function applyEntry() {\n...\n*481: function inferTodoOp() {\n*634: function formatSummary() {",
                "details":{"matchCount":3,"fileCount":1,"scopePath":path,"fileMatches":[{"path":path,"count":3}]}});
        for replay in [false, true] {
            let events = vec![
                AgentEvent::ToolStarted {
                    id: "g".into(),
                    name: "rg".into(),
                    arguments: arguments.to_string(),
                },
                AgentEvent::ToolFinished {
                    id: "g".into(),
                    result: result.clone(),
                },
            ];
            let mut transcript = Transcript::new();
            if replay {
                assert!(
                    transcript
                        .replay(events.into_iter().map(ReplayRecord::Event).collect())
                        .is_none()
                );
            } else {
                for event in events {
                    transcript.progress(event);
                }
            }
            let buffer = render(&mut transcript, 100, 12);
            let text = screen(&buffer);
            let rows: Vec<_> = text
                .lines()
                .skip(1)
                .map(|line| {
                    line.chars()
                        .skip(2)
                        .take(96)
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect();
            assert!(
                rows[0].starts_with(&format!(
                    "{} Grep: function applyEntry|function formatSummary 3 matches · 1 file · in",
                    palette::tool_icon("rg")
                )),
                "{text}"
            );
            assert_eq!(rows[1], path);
            assert_eq!(rows[2], "└─ *399│ function applyEntry() {");
            assert!(text.contains("   *634│ function formatSummary() {"));
            assert!(!text.contains("abc123") && !text.contains(" · ok"));
            assert_eq!(buffer[(0, 0)].fg, palette::rg_border());
            assert_eq!(buffer[(2, 1)].fg, palette::search_title());
            assert_eq!(buffer[(10, 1)].fg, palette::rg_text());
            assert_eq!(buffer[(2, 2)].fg, palette::rg_meta());
            assert_eq!(buffer[(2, 3)].fg, palette::rg_title());
            for y in 0..8 {
                assert_eq!(buffer[(99, y)].bg, palette::rg_bg());
                assert!(
                    !buffer[(0, y)]
                        .modifier
                        .contains(Modifier::DIM | Modifier::BOLD)
                );
            }
            assert_eq!(transcript.entries[0].result, Some(result.clone()));
            for width in [1, 8, 40, 100] {
                // 窄宽度按字符折行，宽度 1 只剩边框列。
                let buffer = render(&mut transcript, width, 100);
                if width > 1 {
                    let text = screen(&buffer);
                    let header: String = text
                        .lines()
                        .skip(1)
                        .take(4)
                        .map(|line| line.trim_matches([' ', '│']))
                        .collect();
                    assert!(header.contains("Grep"), "{width}: {header}");
                }
            }
        }
    }

    for replay in [false, true] {
        for (mode, content, details, summary) in [
            (
                "files_with_matches",
                "# odd.txt\n*123|literal.txt\n",
                json!({"outputMode":"files_with_matches","fileCount":2,"scopePath":".","fileMatches":[{"path":"# odd.txt"},{"path":"*123|literal.txt"}]}),
                "files_with_matches · 2 files",
            ),
            (
                "count",
                "# odd.txt:300\n*123|literal.txt:200\n\nTotal: 900\n",
                json!({"outputMode":"count","matchCount":500,"totalMatchCount":900,"countComplete":true,"fileCount":2,"scopePath":".","fileMatches":[{"path":"# odd.txt","count":300},{"path":"*123|literal.txt","count":200}]}),
                "count 900 matches · 2 files",
            ),
            (
                "count",
                "Total (partial): 0\nCounts are partial\n",
                json!({"outputMode":"count","matchCount":0,"totalMatchCount":0,"countComplete":false,"fileCount":0,"scopePath":".","fileMatches":[]}),
                "count 0 matches (partial) · 0 files",
            ),
        ] {
            let result = json!({"ok":true,"content":content,"details":details});
            let events = vec![
                AgentEvent::ToolStarted {
                    id: "g".into(),
                    name: "rg".into(),
                    arguments: json!({"pattern":"needle","output_mode":mode}).to_string(),
                },
                AgentEvent::ToolFinished {
                    id: "g".into(),
                    result: result.clone(),
                },
            ];
            let mut transcript = Transcript::new();
            if replay {
                assert!(
                    transcript
                        .replay(events.into_iter().map(ReplayRecord::Event).collect())
                        .is_none()
                );
            } else {
                for event in events {
                    transcript.progress(event);
                }
            }
            let text = screen(&render(&mut transcript, 140, 20));
            assert!(text.contains(summary), "{text}");
            assert!(text.contains(content.lines().next().unwrap()), "{text}");
            assert!(!text.contains("└─"), "{text}");
            if mode == "files_with_matches" {
                assert!(
                    !text.contains("0 matches") && !text.contains("2 matches"),
                    "{text}"
                );
                assert!(text.contains("*123|literal.txt"), "{text}");
            }
            assert_eq!(transcript.entries[0].result, Some(result));
        }
    }

    {
        let mut transcript = Transcript::new();
        tool(&mut transcript, "g", "rg", json!({"pattern":"needle"}));
        transcript.progress(AgentEvent::ToolFinished { id:"g".into(), result:json!({
                "ok":true,"content":"# src/\n## a.rs#abc\n*1:needle one\n\n## nested/\n### b.rs#def\n*9|needle two\n\n[truncated by output limits]",
                "details":{"matchCount":2,"fileCount":2,"scopePath":".","fileMatches":[{"path":"src/a.rs"},{"path":"src/nested/b.rs"}]}
            })});
        let preview = screen(&render(&mut transcript, 100, 20))
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(preview.contains("src/a.rs") && preview.contains("└─ *1│needle one"));
        assert!(preview.contains("src/nested/b.rs") && preview.contains("└─ *9│needle two"));
        transcript.toggle_tool_output();
        assert!(screen(&render(&mut transcript, 100, 20)).contains("[truncated by output limits]"));
        tool(&mut transcript, "failed", "rg", json!({"pattern":"bad"}));
        transcript.progress(AgentEvent::ToolFinished {
            id: "failed".into(),
            result: json!({"ok":false,"error":"cannot search path"}),
        });
        let buffer = render(&mut transcript, 100, 25);
        let text = screen(&buffer);
        assert!(text.contains("Grep: bad · failed") && text.contains("cannot search path"));
        let row = text
            .lines()
            .position(|line| line.contains("cannot search path"))
            .unwrap() as u16;
        assert_eq!(buffer[(0, row)].fg, palette::removed());
        assert_eq!(buffer[(0, row)].bg, palette::rg_bg());
    }
}

#[test]
fn glob_and_web_search_cards_render_colors_grouping_and_replay() {
    let result =
        json!({"ok":true,"content":"root.rs\n# src/\nlib.rs\n## tui/\n界面.rs\n# tests/\ncli.rs"});
    for (replay, arguments, header) in [
        (
            false,
            json!({"pattern":"**/*.rs","path":"src"}),
            "Glob: **/*.rs · src",
        ),
        (
            true,
            json!({"pattern":"**/*.rs","path":"src"}),
            "Glob: **/*.rs · src",
        ),
        (true, json!({"path":"**/*.rs"}), "Glob: **/*.rs"),
    ] {
        let events = vec![
            AgentEvent::ToolStarted {
                id: "paths".into(),
                name: "glob".into(),
                arguments: arguments.to_string(),
            },
            AgentEvent::ToolFinished {
                id: "paths".into(),
                result: result.clone(),
            },
        ];
        let mut transcript = Transcript::new();
        if replay {
            assert!(
                transcript
                    .replay(events.into_iter().map(ReplayRecord::Event).collect())
                    .is_none()
            );
        } else {
            for event in events {
                transcript.progress(event);
            }
        }
        let buffer = render(&mut transcript, 60, 16);
        let text = screen(&buffer);
        assert!(text.contains(header) && !text.contains("# src/"), "{text}");
        for (needle, column, color) in [
            ("root.rs", 2, palette::read_summary_path()),
            ("src/", 2, palette::rg_title()),
            ("lib.rs", 4, palette::read_summary_path()),
            ("tui/", 4, palette::rg_title()),
            ("界", 6, palette::read_summary_path()),
            ("tests/", 2, palette::rg_title()),
            ("cli.rs", 4, palette::read_summary_path()),
        ] {
            let row = text
                .lines()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle}: {text}")) as u16;
            assert_eq!(buffer[(column, row)].fg, color, "{needle}");
            assert_eq!(buffer[(0, row)].symbol(), "│");
            assert_eq!(buffer[(59, row)].symbol(), "│");
        }
        assert_eq!(transcript.entries[0].result, Some(result.clone()));
        for width in [1, 2, 4, 10] {
            render(&mut transcript, width, 30);
        }
        render(&mut transcript, 60, 16);
        assert!(entry_text(&mut transcript.entries[0], 56).contains("界面.rs"));
    }

    {
        let mut transcript = Transcript::new();
        transcript.progress(AgentEvent::ToolStarted {
            id: "ws".into(),
            name: "web_search".into(),
            arguments: json!({"query": "rust 1.88", "provider": "public"}).to_string(),
        });
        let running = screen(&render(&mut transcript, 90, 8));
        assert!(
            running.contains("Web Search: rust 1.88 · public · running"),
            "{running}"
        );

        let content = (1..=6)
            .map(|i| format!("[{i}] Title {i}\n    https://example.com/{i}\n    Snippet {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        transcript.progress(AgentEvent::ToolFinished {
            id: "ws".into(),
            result: json!({
                "ok": true,
                "provider": "public",
                "provider_label": "Public Web",
                "content": content,
                "details": {"source_count": 6}
            }),
        });
        let buffer = render(&mut transcript, 90, 14);
        let done = screen(&buffer);
        assert!(
            done.contains("Web Search: rust 1.88 · Public Web · 6 results"),
            "{done}"
        );
        assert!(done.contains("[1] Title 1"), "{done}");
        assert!(done.contains("https://example.com/1"), "{done}");
        assert!(done.contains("more lines (Ctrl + O to expand)"), "{done}");
    }
}
