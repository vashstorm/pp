use super::*;

#[test]
fn bash_background_snapshot_counts_only_output_rows() {
    for name in ["bash", "bash_task"] {
        for count in [0_usize, 3, 20] {
            let mut transcript = Transcript::new();
            tool(
                &mut transcript,
                "snapshot",
                name,
                json!({"mode":"background"}),
            );
            let output = (0..count)
                .map(|i| format!("snapshot-{i:02}\n"))
                .collect::<String>();
            transcript.progress(AgentEvent::ToolFinished {
                id: "snapshot".into(),
                result: json!({"ok":true,"task_id":"bg-1","status":"running","stdout":output,"stderr":""}),
            });
            let rendered = screen(&render(&mut transcript, 100, 30));
            assert_eq!(rendered.matches("Task bg-1: running").count(), 1);
            let entry = &transcript.entries[0];
            assert_eq!(entry.text, output.trim_end_matches('\n'));
            assert_eq!(
                entry.layout.as_ref().unwrap().height,
                4 + count.min(10) + usize::from(count > 10)
            );
        }
    }
}

#[test]
fn activity_waiting_timer_starts_at_each_ask_and_restores_run_timing() {
    let mut transcript = Transcript::new();
    let started = Instant::now() - Duration::from_secs(85);
    transcript.activity = Some(Activity::new(started));
    transcript.tick(started + Duration::from_secs(85));
    assert!(screen(&render(&mut transcript, 100, 30)).contains("Requesting model · 1m 25s"));

    for id in ["ask-1", "ask-2"] {
        transcript.progress(AgentEvent::ToolStarted {
            id: id.into(),
            name: "ask".into(),
            arguments: r#"{"questions":[{"id":"q","question":"Question","options":[]}]}"#.into(),
        });
        transcript.waiting_for_input();
        let waiting = transcript
            .activity
            .as_ref()
            .unwrap()
            .waiting_started
            .unwrap();
        assert!(screen(&render(&mut transcript, 100, 30)).contains("Waiting for input · 0s"));
        assert!(!transcript.tick(waiting + Duration::from_millis(999)));
        transcript.tick(waiting + Duration::from_secs(85));
        assert!(screen(&render(&mut transcript, 100, 30)).contains("Waiting for input · 1m 25s"));
        transcript.progress(AgentEvent::ToolFinished {
            id: id.into(),
            result: json!({"ok":true,"answers":[{"id":"q","selected_options":[],"custom_input":"yes"}]}),
        });
        assert!(
            transcript
                .activity
                .as_ref()
                .unwrap()
                .waiting_started
                .is_none()
        );
        transcript.tick(started + Duration::from_secs(85));
        assert!(
            screen(&render(&mut transcript, 100, 30)).contains("Processing tool result · 1m 25s")
        );
        assert_eq!(transcript.activity.as_ref().unwrap().started, Some(started));
    }
    transcript.waiting_for_input();
    transcript.cancelling();
    transcript.waiting_for_input();
    transcript.tick(started + Duration::from_secs(85));
    assert!(screen(&render(&mut transcript, 100, 30)).contains("Cancelling · 1m 25s"));
    transcript.cancel();
    assert!(!transcript.tick(Instant::now() + Duration::from_secs(3600)));
}

#[test]
fn activity_duration_carries_seconds_minutes_and_hours() {
    let started = Instant::now();
    let mut transcript = Transcript::new();
    transcript.activity = Some(Activity::new(started));
    for (seconds, expected) in [
        (0, "0s"),
        (59, "59s"),
        (60, "1m 0s"),
        (85, "1m 25s"),
        (3599, "59m 59s"),
        (3600, "1h 0m 0s"),
        (3725, "1h 2m 5s"),
        (86400, "24h 0m 0s"),
    ] {
        transcript.tick(started + Duration::from_secs(seconds));
        let text = screen(&render(&mut transcript, 100, 30));
        assert!(
            text.contains(&format!("Requesting model · {expected}")),
            "{text}"
        );
    }
}

#[test]
fn activity_timer_ticks_without_events_and_stops_on_failure() {
    let mut transcript = Transcript::new();
    transcript.append("You", "waiting task");
    transcript.begin_run();
    let started = transcript.activity.as_ref().unwrap().started.unwrap();
    assert!(!transcript.tick(started + Duration::from_millis(999)));
    assert!(transcript.tick(started + Duration::from_secs(1)));
    assert!(!transcript.tick(started + Duration::from_millis(1500)));
    let text = screen(&render(&mut transcript, 100, 30));
    assert!(text.contains("✻ Requesting model · 1s"));
    assert_eq!(transcript.entries.len(), 1);
    transcript.fail();
    assert!(!transcript.tick(started + Duration::from_secs(90)));
    assert!(screen(&render(&mut transcript, 100, 30)).contains("✻ Failed ·"));
}

#[test]
fn activity_follows_latest_message_across_model_and_tool_phases() {
    let mut transcript = Transcript::new();
    transcript.append("You", "run task");
    transcript.begin_run();
    let phases = [
        (AgentEvent::AssistantStarted, "Requesting model"),
        (
            AgentEvent::ThinkingDelta {
                text: "thinking content".into(),
            },
            "Thinking",
        ),
        (
            AgentEvent::AssistantDelta {
                text: "answer content".into(),
                refusal: false,
            },
            "Responding",
        ),
        (
            AgentEvent::AssistantFinished {
                text: "answer content".into(),
            },
            "Processing response",
        ),
        (
            AgentEvent::ToolStarted {
                id: "read-1".into(),
                name: "read".into(),
                arguments: r#"{"path":"test.txt"}"#.into(),
            },
            "Running read",
        ),
        (
            AgentEvent::ToolFinished {
                id: "read-1".into(),
                result: json!({"ok":true,"content":"tool content"}),
            },
            "Processing tool result",
        ),
        (AgentEvent::AssistantStarted, "Requesting model"),
    ];
    for (event, label) in phases {
        transcript.progress(event);
        let started = transcript.activity.as_ref().unwrap().started.unwrap();
        transcript.tick(started + Duration::from_secs(5));
        let text = screen(&render(&mut transcript, 100, 30));
        assert!(text.contains(&format!("✻ {label} · 5s")), "{text}");
        assert_eq!(text.matches('✻').count(), 1);
        assert!(text.find("run task").unwrap() < text.find('✻').unwrap());
        if text.contains("answer content") {
            assert!(text.find("answer content").unwrap() < text.find('✻').unwrap());
        }
    }
    transcript.cancelling();
    transcript.progress(AgentEvent::ThinkingDelta {
        text: "late delta".into(),
    });
    assert!(screen(&render(&mut transcript, 100, 30)).contains("✻ Cancelling ·"));
    transcript.cancel();
    assert!(screen(&render(&mut transcript, 100, 30)).contains("✻ Cancelled ·"));
    transcript.begin_run();
    transcript.append("You", "next task");
    let text = screen(&render(&mut transcript, 100, 30));
    assert_eq!(text.matches('✻').count(), 1);
    assert!(text.contains("✻ Requesting model · 0s"));
    assert!(text.find("next task").unwrap() < text.find('✻').unwrap());
    // 窄屏也只占一行；恢复宽度后显示完整状态。
    let text = screen(&render(&mut transcript, 18, 40));
    assert_eq!(text.lines().filter(|line| line.contains('✻')).count(), 1);
    assert_eq!(
        transcript
            .activity
            .as_ref()
            .unwrap()
            .entry
            .layout
            .as_ref()
            .unwrap()
            .height,
        1
    );
    assert!(screen(&render(&mut transcript, 100, 30)).contains("✻ Requesting model · 0s"));
}

#[test]
fn replay_restores_visible_history_and_marks_interruption() {
    let mut transcript = Transcript::new();
    let error = transcript.replay(vec![
        ReplayRecord::User("remember replay".into()),
        ReplayRecord::Event(AgentEvent::AssistantStarted),
        ReplayRecord::Event(AgentEvent::ToolStarted {
            id: "b1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"printf replay"}"#.into(),
        }),
        ReplayRecord::Event(AgentEvent::ToolFinished {
            id: "b1".into(),
            result: json!({"ok":true,"stdout":"replay\n","exit_code":0}),
        }),
        ReplayRecord::Event(AgentEvent::AssistantDelta {
            text: "partial".into(),
            refusal: false,
        }),
    ]);
    let text = screen(&render(&mut transcript, 100, 30));
    assert!(text.contains("remember replay"));
    assert!(text.contains("replay"));
    assert!(text.contains("Timeout: 120s"));
    assert!(!text.contains("Exit:"));
    assert!(!text.contains("Wall:"));
    assert_eq!(error.as_deref(), Some("Previous request was interrupted."));
    assert!(!text.contains("Previous request was interrupted."));
    assert_eq!(
        transcript.entries.last().unwrap().status,
        EntryStatus::Failed
    );

    // 完整结束的记录不会误报中断。
    let mut transcript = Transcript::new();
    let error = transcript.replay(vec![
        ReplayRecord::User("done task".into()),
        ReplayRecord::Event(AgentEvent::AssistantStarted),
        ReplayRecord::Event(AgentEvent::AssistantFinished {
            text: "final answer".into(),
        }),
        ReplayRecord::Error("task failed".into()),
    ]);
    let text = screen(&render(&mut transcript, 100, 30));
    assert!(text.contains("final answer"));
    assert!(error.is_none());
    assert!(!text.contains("task failed"));
    assert!(!text.contains("Previous request was interrupted."));
}

#[test]
fn replay_cancel_preserves_partial_and_clears_interruption() {
    let mut transcript = Transcript::new();
    let error = transcript.replay(vec![
        ReplayRecord::Error("previous failure".into()),
        ReplayRecord::User("cancelled prompt".into()),
        ReplayRecord::Event(AgentEvent::AssistantStarted),
        ReplayRecord::Event(AgentEvent::AssistantDelta {
            text: "visible partial".into(),
            refusal: false,
        }),
        ReplayRecord::Cancelled,
    ]);
    assert!(error.is_none());
    let text = screen(&render(&mut transcript, 100, 30));
    assert!(text.contains("visible partial"));
    assert!(text.contains(crate::cancellation::CANCELLED_NOTE));
    assert_eq!(transcript.entries[1].status, EntryStatus::Failed);
    assert!(transcript.assistant.is_none());
    assert!(transcript.thinking.is_none());
}

#[test]
fn replay_marks_interruption_before_first_answer_text() {
    for thinking in [false, true] {
        let mut transcript = Transcript::new();
        let mut records = vec![ReplayRecord::Event(AgentEvent::AssistantStarted)];
        if thinking {
            records.push(ReplayRecord::Event(AgentEvent::ThinkingDelta {
                text: "unfinished thought".into(),
            }));
        }
        let error = transcript.replay(records);
        assert_eq!(error.as_deref(), Some("Previous request was interrupted."));
        assert!(!screen(&render(&mut transcript, 100, 30)).contains("interrupted"));
    }
}

#[test]
fn grep_reference_layout_colors_and_replay() {
    let path = "/workspace/pi-coding-agent/src/tools/todo.ts";
    let arguments = json!({"pattern":"function applyEntry|function formatSummary", "path":path});
    let result = json!({"ok":true,
            "content":"[todo.ts#abc123]\n*399: function applyEntry() {\n...\n*481: function inferTodoOp() {\n*634: function formatSummary() {",
            "details":{"matchCount":3,"fileCount":1,"scopePath":path,"fileMatches":[{"path":path,"count":3}]}});
    for replay in [false, true] {
        let events = vec![
            AgentEvent::ToolStarted {
                id: "g".into(),
                name: "grep".into(),
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
                palette::tool_icon("grep")
            )),
            "{text}"
        );
        assert_eq!(rows[1], path);
        assert_eq!(rows[2], "└─ *399│ function applyEntry() {");
        assert!(text.contains("   *634│ function formatSummary() {"));
        assert!(!text.contains("abc123") && !text.contains(" · ok"));
        assert_eq!(buffer[(0, 0)].fg, palette::GREP_BORDER);
        assert_eq!(buffer[(2, 1)].fg, palette::GREP_TITLE);
        assert_eq!(buffer[(10, 1)].fg, palette::GREP_TEXT);
        assert_eq!(buffer[(2, 2)].fg, palette::GREP_META);
        assert_eq!(buffer[(2, 3)].fg, palette::GREP_TITLE);
        for y in 0..8 {
            assert_eq!(buffer[(99, y)].bg, palette::GREP_BG);
            assert!(
                !buffer[(0, y)]
                    .modifier
                    .contains(Modifier::DIM | Modifier::BOLD)
            );
        }
        assert_eq!(transcript.entries[0].result, Some(result.clone()));
        for width in [1, 8, 40, 100] {
            let buffer = render(&mut transcript, width, 100);
            assert!(screen(&buffer).contains("G") || width == 1);
        }
    }
}

#[test]
fn grep_groups_keep_paths_notices_and_failures() {
    let mut transcript = Transcript::new();
    tool(&mut transcript, "g", "grep", json!({"pattern":"needle"}));
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
    tool(&mut transcript, "failed", "grep", json!({"pattern":"bad"}));
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
    assert_eq!(buffer[(0, row)].fg, palette::REMOVED);
    assert_eq!(buffer[(0, row)].bg, palette::GREP_BG);
}

#[test]
fn glob_card_groups_paths_and_replays_without_changing_results() {
    let result =
        json!({"ok":true,"content":"root.rs\n# src/\nlib.rs\n## tui/\n界面.rs\n# tests/\ncli.rs"});
    for replay in [false, true] {
        let events = vec![
            AgentEvent::ToolStarted {
                id: "paths".into(),
                name: "glob".into(),
                arguments: json!({"path":"**/*.rs"}).to_string(),
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
        assert!(
            text.contains("Glob: **/*.rs") && !text.contains("# src/"),
            "{text}"
        );
        for (needle, column, color) in [
            ("root.rs", 2, palette::READ_SUMMARY_PATH),
            ("src/", 2, palette::GREP_TITLE),
            ("lib.rs", 4, palette::READ_SUMMARY_PATH),
            ("tui/", 4, palette::GREP_TITLE),
            ("界", 6, palette::READ_SUMMARY_PATH),
            ("tests/", 2, palette::GREP_TITLE),
            ("cli.rs", 4, palette::READ_SUMMARY_PATH),
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
}

#[test]
fn bash_card_distinguishes_live_stderr_and_failed_status_from_stdout() {
    let mut transcript = Transcript::new();
    tool(
        &mut transcript,
        "b",
        "bash",
        json!({"command":"cargo check"}),
    );
    for (stream, text) in [
        (OutputStream::Stdout, "checking crate\n"),
        (OutputStream::Stderr, "compiler diagnostic\n"),
    ] {
        transcript.progress(AgentEvent::ToolOutput {
            id: "b".into(),
            stream,
            text: text.into(),
        });
    }
    let live = render(&mut transcript, 80, 20);
    let text = screen(&live);
    assert!(text.contains("Bash · running"));
    let row = text
        .lines()
        .position(|line| line.contains("compiler diagnostic"))
        .unwrap() as u16;
    assert_eq!(live[(2, row)].fg, palette::ACCENT);
    transcript.progress(AgentEvent::ToolFinished { id: "b".into(), result: json!({"ok":false,"exit_code":1,"stdout":"checking crate\n","stderr":"compiler diagnostic\n"}) });
    let failed = render(&mut transcript, 80, 20);
    let text = screen(&failed);
    assert!(text.contains("Bash · failed") && text.contains("Exit: 1"));
    assert_eq!(failed[(0, 0)].fg, palette::REMOVED);
    for (needle, color) in [
        ("checking crate", palette::GREP_TEXT),
        ("compiler diagnostic", palette::REMOVED),
        ("Exit: 1", palette::MUTED),
    ] {
        let row = text.lines().position(|line| line.contains(needle)).unwrap() as u16;
        assert_eq!(failed[(2, row)].fg, color);
    }
    assert_eq!(text.matches("compiler diagnostic").count(), 1);
}

#[test]
fn streaming_category_colors_and_statuses_preserve_default_body() {
    for kind in [EntryKind::Grep, EntryKind::Data] {
        let mut transcript = Transcript::new();
        let tool = kind == EntryKind::Grep;
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
        assert_eq!(buffer[(0, 0)].fg, kind.color());
        assert_eq!(buffer[(2, 0)].fg, kind.color());
        assert!(buffer[(2, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(4, 1)].fg, Color::Reset);
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
    assert_eq!(buffer[(0, 0)].bg, USER_BACKGROUND);
    assert_eq!(buffer[(2, 0)].bg, USER_BACKGROUND);
    assert_eq!(buffer[(2, 1)].symbol(), "u");
    assert_eq!(buffer[(2, 1)].bg, USER_BACKGROUND);
    assert_eq!(buffer[(0, 2)].symbol(), " ");
    assert_eq!(buffer[(0, 2)].bg, USER_BACKGROUND);
    assert_eq!(buffer[(0, 4)].bg, ASSISTANT_BACKGROUND);
    assert_eq!(buffer[(2, 4)].bg, ASSISTANT_BACKGROUND);
    assert_ne!(USER_BACKGROUND, ASSISTANT_BACKGROUND);
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
    assert_eq!(buffer[(2, 0)].fg, Color::Reset);
    assert_eq!(buffer[(2, 0)].bg, ASSISTANT_BACKGROUND);
    assert_eq!(buffer[(2, 2)].fg, MUTED_TEXT);
    assert_eq!(buffer[(2, 2)].bg, ASSISTANT_BACKGROUND);
}

#[test]
fn tool_icons_are_distinct_and_keep_two_column_prefixes_across_card_styles() {
    let names = [
        "read",
        "edit",
        "write",
        "bash_task",
        "grep",
        "find",
        "ask",
        "plan_search",
        "plan_find",
        "plan_write",
        "plan_submit",
        "mcp__fixture__tool",
        "unknown_tool",
    ];
    let mut icons = std::collections::HashSet::new();
    for name in names {
        let icon = palette::tool_icon(name);
        assert!(icons.insert(icon), "工具图标应独立：{name}");
        assert_eq!(Span::raw(icon).width(), 1, "图标应占一列：{name}");
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
                // 普通 Read 摘要以实心圆替代文件图标，仍保留两列前缀。
                let display_icon = if name == "read" { "●" } else { icon };
                let row = (0..buffer.area.height)
                    .find_map(|row| {
                        (0..buffer.area.width)
                            .find(|&column| buffer[(column, row)].symbol() == display_icon)
                            .map(|column| (column, row))
                    })
                    .unwrap_or_else(|| panic!("工具标题应显示图标：{name}\n{}", screen(&buffer)));
                assert_eq!(buffer[(row.0 + 1, row.1)].symbol(), " ");
                if width == 80 {
                    assert_eq!(
                        buffer[(row.0 + 2, row.1)].symbol(),
                        if name == "read" {
                            "R".into()
                        } else if name == "grep" {
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
fn bash_footer_tracks_elapsed_and_renders_status_on_bottom_border() {
    for (result, expected, border) in [
        (
            json!({"ok":true,"exit_code":0,"stdout":"done"}),
            "Wall:",
            BASH_BORDER,
        ),
        (
            json!({"ok":true,"exit_code":100,"stdout":"failed","artifact_id":227}),
            "Artifact: 227 · Exit: 100",
            palette::REMOVED,
        ),
        (
            json!({"ok":false,"exit_code":null,"timed_out":true}),
            "timed out",
            palette::REMOVED,
        ),
        (
            json!({"ok":false,"exit_code":null,"cancelled":true}),
            "cancelled",
            palette::REMOVED,
        ),
    ] {
        let mut transcript = Transcript::new();
        transcript.set_bash_timeout(300);
        assert_eq!(transcript.fresh().bash_timeout_seconds, 300);
        tool(
            &mut transcript,
            "b",
            "bash",
            json!({"command":"cargo test"}),
        );
        let live = screen(&render(&mut transcript, 100, 20));
        assert!(live.contains("╰─ Elapsed: 0s · Timeout: 300s"), "{live}");
        assert!(!live.contains("Wall:") && !live.contains("Exit:"));
        let started = transcript.entries[0].started.unwrap();
        assert!(transcript.tick(started + Duration::from_secs(8)));
        let live = screen(&render(&mut transcript, 100, 20));
        assert!(live.contains("Elapsed: 8s · Timeout: 300s"), "{live}");
        assert!(!transcript.tick(started + Duration::from_millis(8500)));
        transcript.progress(AgentEvent::ToolFinished {
            id: "b".into(),
            result: result.clone(),
        });
        let buffer = render(&mut transcript, 100, 20);
        let text = screen(&buffer);
        let row = text
            .lines()
            .position(|line| line.contains("Wall:"))
            .unwrap() as u16;
        assert!(text.contains(expected), "{text}");
        assert!(text.contains("Timeout: 300s"), "{text}");
        assert!(!text.contains("Elapsed:") && !text.contains("Exit: 0"));
        assert_eq!(buffer[(0, row)].symbol(), "╰");
        assert_eq!(buffer[(99, row)].symbol(), "╯");
        for y in 0..=row {
            assert_eq!(buffer[(0, y)].fg, border);
            assert_eq!(buffer[(99, y)].fg, border);
        }
        assert_eq!(
            text.contains("Artifact:"),
            result.get("artifact_id").is_some()
        );
    }
}

#[test]
fn bash_background_footer_does_not_invent_a_timeout() {
    let mut transcript = Transcript::new();
    tool(
        &mut transcript,
        "bg",
        "bash",
        json!({"command":"serve","mode":"background"}),
    );
    transcript.progress(AgentEvent::ToolFinished {
        id: "bg".into(),
        result: json!({"ok":true,"task_id":"task-1","status":"running","exit_code":null}),
    });
    let text = screen(&render(&mut transcript, 80, 20));
    assert!(text.contains("Task task-1: running"));
    assert!(!text.contains("Timeout:") && !text.contains("Exit:"));
}

#[test]
fn bash_card_and_thinking_match_reference_and_keep_full_output() {
    let mut long_command = Transcript::new();
    tool(
        &mut long_command,
        "command",
        "bash",
        json!({"command":(0..30).map(|i|format!("command-{i:02}")).collect::<Vec<_>>().join("\n")}),
    );
    let command_buffer = render(&mut long_command, 100, 30);
    let command = screen(&command_buffer);
    assert_eq!(command_buffer[(2, 1)].symbol(), "…");
    assert!(command.contains("command-29"));
    assert!(!command.contains("command-00"));
    let mut transcript = Transcript::new();
    tool(&mut transcript, "b", "bash", json!({"command":"git diff"}));
    let output: String = (0..24).map(|i| format!("line-{i:02}\n")).collect();
    transcript.progress(AgentEvent::ToolFinished {
        id: "b".into(),
        result: json!({"ok":true,"stdout":output,"exit_code":0}),
    });
    transcript.progress(AgentEvent::ThinkingDelta {
        text: "Check the diff 中😀".into(),
    });
    let buffer = render(&mut transcript, 100, 30);
    let text = screen(&buffer);
    assert_eq!(buffer[(2, 1)].symbol(), "$");
    for expected in [
        "╭─",
        "$ git diff",
        "├─ Output ─",
        "14 earlier lines, showing 10 of 24",
        "Ctrl + O to expand",
        "line-14",
        "line-23",
        "Wall:",
        "Timeout: 120s",
        "╰─",
        "Check the diff 中",
    ] {
        assert!(text.contains(expected), "{text}");
    }
    assert!(!text.contains("line-00"));
    assert_eq!(buffer[(0, 0)].fg, BASH_BORDER);
    assert_eq!(buffer[(2, 1)].fg, palette::ADDED);
    assert_eq!(buffer[(4, 1)].fg, palette::TOOL_COMMAND);
    assert!(text.contains("Bash · done"));
    assert_eq!(buffer[(2, 1)].bg, BASH_BACKGROUND);
    let think_row = text
        .lines()
        .position(|line| line.contains("Check the diff"))
        .unwrap() as u16;
    assert_eq!(buffer[(0, think_row)].symbol(), " ");
    assert_eq!(buffer[(2, think_row)].fg, MUTED_TEXT);
    assert_eq!(buffer[(2, think_row)].bg, ASSISTANT_BACKGROUND);
    transcript.toggle_tool_output();
    let expanded_buffer = render(&mut transcript, 100, 40);
    assert_eq!(expanded_buffer[(2, 1)].symbol(), "$");
    let expanded = screen(&expanded_buffer);
    assert!(expanded.contains("line-00"));
    assert!(!expanded.contains("earlier lines"));
    transcript.toggle_tool_output();
    render(&mut transcript, 5, 2);
    let snapshot = render(&mut transcript, 100, 30);
    let mut replay = Vec::new();
    transcript
        .write_full_for_test(&mut replay, &snapshot)
        .unwrap();
    let mut parser = vt100::Parser::new(80, 100, 0);
    parser.process(&replay);
    assert!(parser.screen().contents().contains("line-00"));
    for width in [1, 2, 3, 4, 10] {
        render(&mut transcript, width, 8);
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
    assert_eq!(buffer[plain].fg, MUTED_TEXT);
    assert_eq!(buffer[plain].bg, ASSISTANT_BACKGROUND);
    // 粗体叠加 BOLD modifier，fg 仍继承灰色基色。
    let bold = cell_of("bold");
    assert_eq!(buffer[bold].fg, MUTED_TEXT);
    assert!(buffer[bold].modifier.contains(Modifier::BOLD));
    // 行内代码使用 CODE 色。
    let code = cell_of("code");
    assert_eq!(buffer[code].fg, palette::CODE);
    assert_eq!(buffer[code].bg, ASSISTANT_BACKGROUND);
    // 围栏代码块按 syntect 语义高亮，`let` 为 keyword 色。思考正文的行级灰
    // 基调被语义 span 覆盖，与粗体/行内代码的约定一致。
    let block = cell_of("let x = 1;");
    assert_eq!(buffer[block].fg, palette::SYNTAX_KEYWORD);
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
    assert_eq!(buffer[(column, row)].fg, MUTED_TEXT);
    assert!(buffer[(column, row)].modifier.contains(Modifier::BOLD));
    assert!(text.contains("tail"));
}

#[test]
fn streaming_scrolling_keeps_viewport_until_follow_is_restored() {
    let mut transcript = Transcript::new();
    for index in 0..60 {
        transcript.append("Assistant", &format!("history-{index:02}"));
    }
    assert!(screen(&render(&mut transcript, 40, 8)).contains("history-59"));
    transcript.page_up();
    let before = render(&mut transcript, 40, 8);
    let offset = transcript.offset;
    transcript.progress(AgentEvent::AssistantStarted);
    delta(&mut transcript, "live-first", false);
    let after = render(&mut transcript, 40, 8);
    assert_eq!(transcript.offset, offset);
    assert_eq!(before, after);
    assert!(!transcript.follow_bottom);
    while !transcript.follow_bottom {
        transcript.page_down();
    }
    assert!(screen(&render(&mut transcript, 40, 8)).contains("live-first"));
    delta(&mut transcript, "-last", false);
    assert!(screen(&render(&mut transcript, 40, 8)).contains("live-first-last"));
}

#[test]
fn mouse_scroll_lines_clamp_and_restore_follow() {
    let mut transcript = Transcript::new();
    let history = (0..60)
        .map(|index| format!("line-{index:02}-abcdefghijk"))
        .collect::<Vec<_>>()
        .join("\n");
    delta(&mut transcript, &history, false);
    let bottom = screen(&render(&mut transcript, 12, 8));
    assert!(bottom.contains("line-59"));
    transcript.scroll_up(3);
    let older = screen(&render(&mut transcript, 12, 8));
    assert_eq!(
        older.lines().skip(3).collect::<Vec<_>>(),
        bottom.lines().take(5).collect::<Vec<_>>()
    );
    assert!(!older.contains("line-59"));
    delta(&mut transcript, "\nnew-a\nnew-b", false);
    assert_eq!(screen(&render(&mut transcript, 12, 8)), older);
    assert!(!transcript.follow_bottom);

    transcript.scroll_up(usize::MAX);
    let top = screen(&render(&mut transcript, 12, 8));
    assert!(top.starts_with("  line-00"));
    assert!(top.contains("line-00"));
    transcript.scroll_up(3);
    assert_eq!(screen(&render(&mut transcript, 12, 8)), top);
    transcript.scroll_down(usize::MAX);
    assert!(screen(&render(&mut transcript, 12, 8)).contains("new-b"));
    assert!(transcript.follow_bottom);
    delta(&mut transcript, "\nfollow", false);
    assert!(screen(&render(&mut transcript, 12, 8)).contains("follow"));

    let resized = screen(&render(&mut transcript, 9, 5));
    transcript.scroll_up(3);
    let older = screen(&render(&mut transcript, 9, 5));
    assert_eq!(
        older.lines().skip(3).collect::<Vec<_>>(),
        resized.lines().take(2).collect::<Vec<_>>()
    );
    transcript.scroll_down(usize::MAX);
    assert_eq!(screen(&render(&mut transcript, 9, 5)), resized);

    for text in ["", "short"] {
        let mut transcript = Transcript::new();
        if !text.is_empty() {
            delta(&mut transcript, text, false);
        }
        let original = render(&mut transcript, 20, 8);
        transcript.scroll_up(usize::MAX);
        assert_eq!(render(&mut transcript, 20, 8), original);
        transcript.scroll_down(usize::MAX);
        assert_eq!(render(&mut transcript, 20, 8), original);
        assert!(transcript.follow_bottom);
    }
}

#[test]
fn streaming_resize_reflows_without_losing_text_or_follow_state() {
    let mut transcript = Transcript::new();
    transcript.append("You", "draft submitted 中😀");
    transcript.progress(AgentEvent::AssistantStarted);
    delta(&mut transcript, "first\n  indented 中😀\n\nlast", false);
    let before = render(&mut transcript, 40, 8);
    render(&mut transcript, 5, 2);
    let after = render(&mut transcript, 40, 8);
    assert_eq!(before, after);
    assert_eq!(transcript.entries[1].text, "first\n  indented 中😀\n\nlast");
    transcript.page_up();
    render(&mut transcript, 20, 4);
    assert!(!transcript.follow_bottom);
    assert!(transcript.offset <= transcript.total.saturating_sub(4));
}

#[test]
fn streaming_completion_preserves_exact_markdown_layout_when_visible_text_is_unchanged() {
    let _worker = crate::tui::render_worker::Worker::start().unwrap();
    let text = format!(
        "# Preserved heading\n\n{}\n```rust\nlet value = 1;\n```",
        "**正文** with Unicode 🙂 and `inline code`.\n\n".repeat(200)
    );
    assert!(text.len() > 8192);
    for refusal in [false, true] {
        let mut transcript = Transcript::new();
        transcript.progress(AgentEvent::AssistantStarted);
        delta(&mut transcript, &text, refusal);
        let entry = &mut transcript.entries[0];
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            entry.ensure_layout(80);
            if entry.render.exact {
                break;
            }
            assert!(Instant::now() < deadline, "等待流式 Markdown 排版超时");
            std::thread::yield_now();
        }
        let layout = entry.layout.as_ref().unwrap().clone();
        let before = entry_text(entry, 80);
        assert!(before.contains("Preserved heading"));
        assert!(!before.contains("**"));
        assert!(!before.contains("```"));

        transcript.progress(AgentEvent::AssistantFinished { text: text.clone() });

        let entry = &mut transcript.entries[0];
        let after = entry_text(entry, 80);
        assert_eq!(entry.status, EntryStatus::Complete);
        assert!(entry.refusal.is_empty());
        assert!(entry.render.exact, "相同正文完成后应保留精确排版");
        assert!(Arc::ptr_eq(&layout, entry.layout.as_ref().unwrap()));
        assert_eq!(before, after);
    }
}

#[test]
fn streaming_completion_refusal_and_empty_turns_do_not_duplicate_cards() {
    let mut transcript = Transcript::new();
    transcript.progress(AgentEvent::AssistantStarted);
    transcript.progress(AgentEvent::AssistantFinished {
        text: String::new(),
    });
    assert!(transcript.entries.is_empty());
    transcript.progress(AgentEvent::AssistantStarted);
    transcript.progress(AgentEvent::AssistantFinished {
        text: "without delta".into(),
    });
    assert_eq!(transcript.entries.len(), 1);
    assert_eq!(
        screen(&render(&mut transcript, 60, 15))
            .matches("without delta")
            .count(),
        1
    );
    transcript.progress(AgentEvent::AssistantStarted);
    delta(&mut transcript, "provisional refusal", true);
    assert!(screen(&render(&mut transcript, 60, 15)).contains("provisional refusal"));
    delta(&mut transcript, "content wins", false);
    let text = screen(&render(&mut transcript, 60, 15));
    assert!(text.contains("content wins"));
    assert!(!text.contains("provisional refusal"));
    transcript.progress(AgentEvent::AssistantFinished {
        text: "content wins".into(),
    });
    assert_eq!(transcript.entries.len(), 2);
    transcript.progress(AgentEvent::AssistantStarted);
    delta(&mut transcript, "only refusal", true);
    transcript.progress(AgentEvent::AssistantFinished {
        text: "only refusal".into(),
    });
    assert_eq!(
        screen(&render(&mut transcript, 60, 15))
            .matches("only refusal")
            .count(),
        1
    );
    transcript.progress(AgentEvent::AssistantStarted);
    delta(&mut transcript, "partial visible", false);
    transcript.fail();
    assert_eq!(transcript.entries[3].status, EntryStatus::Failed);
    let text = screen(&render(&mut transcript, 60, 20));
    assert!(text.contains("partial visible"));
    assert!(!text.contains("Error"));
    assert!(!text.contains("stream failed"));
    assert_eq!(transcript.entries.len(), 4);
}

#[test]
fn streaming_tool_channels_results_and_reused_ids_are_calibrated() {
    let mut transcript = Transcript::new();
    tool(
        &mut transcript,
        "call",
        "bash",
        json!({"command": "grep foo"}),
    );
    assert_eq!(transcript.entries[0].kind, EntryKind::Bash);
    for (stream, text) in [
        (OutputStream::Stdout, "out-first"),
        (OutputStream::Stderr, "err-first"),
    ] {
        transcript.progress(AgentEvent::ToolOutput {
            id: "call".into(),
            stream,
            text: text.into(),
        });
    }
    let live = screen(&render(&mut transcript, 70, 15));
    assert!(live.contains(" Output "));
    assert!(!live.contains("[stdout]"));
    assert!(live.contains("[stderr]"));
    assert!(live.contains("out-first"));
    assert!(live.contains("err-first"));
    transcript.progress(AgentEvent::ToolFinished {
        id: "call".into(),
        result: json!({
            "ok": false, "stdout": "out-firstout-last", "stderr": "err-first",
            "exit_code": 9, "stdout_truncated": true, "stderr_truncated": false,
        }),
    });
    let complete = screen(&render(&mut transcript, 70, 15));
    assert_eq!(complete.matches("out-first").count(), 1);
    assert_eq!(complete.matches("err-first").count(), 1);
    assert!(complete.contains("Exit: 9"));
    assert_eq!(transcript.entries[0].status, EntryStatus::Failed);
    tool(&mut transcript, "call", "bash", json!({"command": "true"}));
    transcript.progress(AgentEvent::ToolFinished {
        id: "call".into(),
        result: json!({"ok": true, "stdout": "", "stderr": "", "exit_code": 0}),
    });
    assert_eq!(transcript.entries[0].status, EntryStatus::Failed);
    assert_eq!(transcript.entries[1].status, EntryStatus::Complete);
    assert_eq!(transcript.entries[1].text, "");
    assert!(transcript.entries[1].footer.contains("Timeout: 120s"));
    assert!(!transcript.entries[1].footer.contains("Exit:"));
    tool(&mut transcript, "read", "read", json!({"path": "SKILL.md"}));
    transcript.progress(AgentEvent::ToolFinished {
        id: "read".into(),
        result: json!({"ok": true, "content": "read contents"}),
    });
    assert_eq!(transcript.entries[2].kind, EntryKind::Data);
    assert_eq!(transcript.entries[2].text, "read contents");
    tool(
        &mut transcript,
        "write",
        "write",
        json!({"path": "file", "content": "private write payload"}),
    );
    transcript.progress(AgentEvent::ToolFinished {
        id: "write".into(),
        result: json!({"ok": true, "bytes_written": 21}),
    });
    assert_eq!(transcript.entries[3].text, "Wrote 21 bytes");
    assert!(
        !transcript.entries[3]
            .title
            .contains("private write payload")
    );
    assert!(
        !tool_result(
            EntryKind::Bash,
            "bash",
            &json!({"ok": false, "error": "spawn failed"})
        )
        .contains("Exit code")
    );
}

#[test]
fn markdown_visual_rows_cross_u16_and_scrolled_history_stays_anchored() {
    let mut transcript = Transcript::new();
    let text = format!("```text\n{}long-entry-tail\n```", "line\n".repeat(70_000));
    transcript.append("Assistant", &text);
    let bottom = render(&mut transcript, 30, 5);
    assert!(transcript.offset > usize::from(u16::MAX));
    assert!(screen(&bottom).contains("long-entry-tail"));
    transcript.progress(AgentEvent::AssistantStarted);
    delta(&mut transcript, "active", false);
    render(&mut transcript, 30, 5);
    transcript.page_up();
    let before = screen(&render(&mut transcript, 30, 5));
    let offset = transcript.offset;
    delta(&mut transcript, "-after-scroll", false);
    assert_eq!(screen(&render(&mut transcript, 30, 5)), before);
    assert_eq!(transcript.offset, offset);
}

#[test]
fn streaming_single_long_logical_line_uses_bounded_grapheme_windows() {
    let mut transcript = Transcript::new();
    transcript.append("Assistant", &format!("{}tail中😀", "a".repeat(140_000)));
    let buffer = render(&mut transcript, 3, 8);
    assert!(transcript.offset > usize::from(u16::MAX));
    let narrow = screen(&buffer);
    assert!(narrow.contains('t'));
    assert!(narrow.contains('l'));
    let layout = transcript.entries[0].layout.as_ref().unwrap();
    assert!(
        layout
            .lines
            .iter()
            .all(|line| line.height <= usize::from(u16::MAX))
    );
    // CJK and emoji need two cells; show the same tail at a useful width.
    assert!(screen(&render(&mut transcript, 30, 5)).contains("tail中"));
}
