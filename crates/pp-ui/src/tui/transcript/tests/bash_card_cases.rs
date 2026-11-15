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
fn bash_card_keeps_stdout_gray_and_stderr_red_and_marks_failed_status() {
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
    assert_eq!(live[(2, row)].fg, palette::removed());
    transcript.progress(AgentEvent::ToolFinished { id: "b".into(), result: json!({"ok":false,"exit_code":1,"stdout":"checking crate\n","stderr":"compiler diagnostic\n"}) });
    let failed = render(&mut transcript, 80, 20);
    let text = screen(&failed);
    assert!(text.contains("Bash · failed") && text.contains("Exit: 1"));
    assert_eq!(failed[(0, 0)].fg, palette::removed());
    let output_row = text
        .lines()
        .position(|line| line.contains(" Output "))
        .unwrap() as u16;
    assert_eq!(failed[(3, output_row)].fg, palette::text());
    for (needle, color) in [
        ("checking crate", palette::muted()),
        ("compiler diagnostic", palette::removed()),
        ("Exit: 1", palette::muted()),
    ] {
        let row = text.lines().position(|line| line.contains(needle)).unwrap() as u16;
        assert_eq!(failed[(2, row)].fg, color);
    }
    assert_eq!(text.matches("compiler diagnostic").count(), 1);
}

#[test]
fn bash_footer_uses_per_call_timeout_and_renders_unlimited() {
    for (seconds, expected) in [
        (0, "Timeout: none"),
        (1, "Timeout: 1s"),
        (3600, "Timeout: 3600s"),
    ] {
        let mut transcript = Transcript::new();
        transcript.set_bash_timeout(37);
        tool(
            &mut transcript,
            "b",
            "bash",
            json!({"command":"sleep 10","timeout_seconds":seconds}),
        );
        let live = screen(&render(&mut transcript, 100, 20));
        assert!(live.contains(expected), "{live}");
        assert!(!live.contains("Timeout: 37s"), "{live}");
        transcript.progress(AgentEvent::ToolFinished {
            id: "b".into(),
            result: json!({"ok":true,"exit_code":0,"output":"done"}),
        });
        let text = screen(&render(&mut transcript, 100, 20));
        assert!(text.contains(expected), "{text}");
    }
}

#[test]
fn bash_footer_tracks_elapsed_and_renders_status_on_bottom_border() {
    for (result, expected, border) in [
        (
            json!({"ok":true,"exit_code":0,"stdout":"done"}),
            "Wall:",
            bash_border(),
        ),
        (
            json!({"ok":true,"exit_code":100,"stdout":"failed","artifact_id":227}),
            "Artifact: 227 · Exit: 100",
            palette::removed(),
        ),
        (
            json!({"ok":false,"exit_code":null,"timed_out":true}),
            "timed out",
            palette::removed(),
        ),
        (
            json!({"ok":false,"exit_code":null,"cancelled":true}),
            "cancelled",
            palette::removed(),
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
fn bash_pipeline_fragments_update_card_and_final_output_replaces_preview() {
    let mut transcript = Transcript::new();
    tool(
        &mut transcript,
        "p",
        "bash",
        json!({"command":"work | tail -30"}),
    );
    for preview in ["中", "中文", "中文片段"] {
        transcript.progress(AgentEvent::ToolPipelineSnapshot {
            id: "p".into(),
            pipes: json!({}),
            preview: Some(preview.into()),
        });
        let text = screen(&render(&mut transcript, 100, 20)).replace(' ', "");
        assert_eq!(text.matches(preview).count(), 1, "{text}");
        assert_eq!(transcript.entries[0].status, EntryStatus::Running);
    }
    transcript.progress(AgentEvent::ToolOutputSnapshot {
        id: "p".into(),
        output: "真实输出".into(),
    });
    let text = screen(&render(&mut transcript, 100, 20)).replace(' ', "");
    assert!(
        text.contains("真实输出") && !text.contains("中文片段"),
        "{text}"
    );
    transcript.progress(AgentEvent::ToolPipelineSnapshot {
        id: "p".into(),
        pipes: json!({}),
        preview: Some("继续增长".into()),
    });
    let text = screen(&render(&mut transcript, 100, 20)).replace(' ', "");
    assert!(text.contains("继续增长"), "{text}");
    transcript.progress(AgentEvent::ToolFinished {
        id: "p".into(),
        result: json!({"ok":true,"exit_code":0,"output":"最终结果\n"}),
    });
    transcript.progress(AgentEvent::ToolPipelineSnapshot {
        id: "p".into(),
        pipes: json!({}),
        preview: Some("迟到片段".into()),
    });
    let text = screen(&render(&mut transcript, 100, 20)).replace(' ', "");
    assert!(text.contains("最终结果"), "{text}");
    assert!(
        !text.contains("继续增长") && !text.contains("迟到片段"),
        "{text}"
    );
}

#[test]
fn bash_footer_surfaces_pipeline_badge_and_warnings_outside_output() {
    const RUN: &str = "0123456789abcdef0123456789abcdef";
    let mut transcript = Transcript::new();
    tool(
        &mut transcript,
        "p",
        "bash",
        json!({"command":"{ printf 'hidden\\nvisible\\n'; exit 7; } | tail -n 1"}),
    );
    transcript.progress(AgentEvent::ToolPipelineSnapshot {
        id: "p".into(),
        preview: None,
        pipes: json!({
            "run_id": RUN,
            "uri": format!("pipe://{RUN}"),
            "pipelines": [{"id": 1, "status": null, "stages": []}],
            "streams": [],
            "nonzero_stages": 1,
            "incomplete": false,
        }),
    });
    let buffer = render(&mut transcript, 120, 20);
    let text = screen(&buffer);
    assert!(text.contains("Non-zero stage results: 1"), "{text}");
    assert!(text.contains("Pipes: /pipes (F8)"), "{text}");
    // 摘要只占底部边框，不进主输出预览。
    let entry = &transcript.entries[0];
    assert!(!entry.text.contains("Non-zero") && !entry.text.contains("/pipes"));
    assert_eq!(entry.footer_warning, "Non-zero stage results: 1");
    let row = text
        .lines()
        .position(|line| line.contains("Non-zero"))
        .unwrap() as u16;
    let column = text
        .lines()
        .nth(usize::from(row))
        .unwrap()
        .find("Non-zero")
        .unwrap() as u16;
    assert_eq!(buffer[(column, row)].fg, palette::accent());

    // 最终 result.pipes 是权威：非零阶段归零后警示消失，入口仍在。
    transcript.progress(AgentEvent::ToolFinished {
        id: "p".into(),
        result: json!({
            "ok": true,
            "exit_code": 0,
            "stdout": "visible\n",
            "pipes": {
                "run_id": RUN,
                "uri": format!("pipe://{RUN}"),
                "pipelines": [{"id": 1, "status": 0, "stages": []}],
                "nonzero_stages": 0,
                "incomplete": false,
            },
        }),
    });
    let text = screen(&render(&mut transcript, 120, 20));
    assert!(text.contains("Pipes: /pipes (F8)"), "{text}");
    assert!(!text.contains("Non-zero stage results"), "{text}");
    assert!(transcript.entries[0].footer_warning.is_empty());
    assert_eq!(transcript.entries[0].status, EntryStatus::Complete);
}

#[test]
fn bash_footer_ignores_old_results_without_pipes() {
    let mut transcript = Transcript::new();
    tool(
        &mut transcript,
        "old",
        "bash",
        json!({"command":"echo old"}),
    );
    transcript.progress(AgentEvent::ToolFinished {
        id: "old".into(),
        result: json!({"ok":true,"exit_code":0,"stdout":"old\n"}),
    });
    let text = screen(&render(&mut transcript, 100, 20));
    assert!(text.contains("Wall:"), "{text}");
    assert!(
        !text.contains("Pipes:") && !text.contains("/pipes"),
        "{text}"
    );
    assert!(transcript.entries[0].footer_warning.is_empty());
}

#[test]
fn restored_bash_result_keeps_pipeline_status_but_marks_bodies_unavailable() {
    const RUN: &str = "fedcba9876543210fedcba9876543210";
    let mut transcript = Transcript::new();
    let now = Instant::now();
    transcript.progress_recorded(
        AgentEvent::ToolStarted {
            id: "restored".into(),
            name: "bash".into(),
            arguments: json!({"command":"false | true"}).to_string(),
        },
        Some(1_000),
        2_000,
        now,
    );
    transcript.progress_recorded(
        AgentEvent::ToolFinished {
            id: "restored".into(),
            result: json!({
                "ok": true,
                "exit_code": 0,
                "stdout": "",
                "pipes": {
                    "run_id": RUN,
                    "uri": format!("pipe://{RUN}"),
                    "pipelines": [{"id": 1, "status": 0, "stages": []}],
                    "nonzero_stages": 1,
                    "incomplete": false,
                },
            }),
        },
        Some(1_500),
        2_000,
        now,
    );
    let entry = &transcript.entries[0];
    assert_eq!(entry.status, EntryStatus::Complete);
    assert!(
        entry.footer.contains("Pipes: /pipes (F8)"),
        "{}",
        entry.footer
    );
    assert_eq!(
        entry.pipes_badge.as_ref().unwrap().status.uri,
        format!("pipe://{RUN}")
    );
    assert!(
        entry.footer.contains("temporary pipe bodies unavailable"),
        "{}",
        entry.footer
    );
    assert_eq!(entry.footer_warning, "Non-zero stage results: 1");
    let text = screen(&render(&mut transcript, 140, 20));
    assert!(text.contains("Non-zero stage results: 1"), "{text}");
    assert!(text.contains("temporary pipe bodies unavailable"), "{text}");
}

#[test]
fn background_bash_pipeline_badge_tracks_status_snapshots() {
    const RUN: &str = "aaaabbbbccccddddaaaabbbbccccdddd";
    let mut transcript = Transcript::new();
    tool(
        &mut transcript,
        "bg",
        "bash",
        json!({"command":"service | tail -n 1","mode":"background"}),
    );
    transcript.progress(AgentEvent::ToolFinished {
        id: "bg".into(),
        result: json!({
            "ok": true,
            "task_id": "task-1",
            "status": "running",
            "exit_code": null,
            "pipes": {
                "run_id": RUN,
                "uri": format!("pipe://{RUN}"),
                "pipelines": [{"id": 1, "status": null, "stages": []}],
                "nonzero_stages": 0,
            },
        }),
    });
    assert!(transcript.entries[0].footer.contains("Pipes: /pipes (F8)"));
    assert!(transcript.entries[0].footer_warning.is_empty());

    // status/wait 快照与启动结果共用同一 badge；正文仍只有后台输出预览。
    tool(&mut transcript, "wait", "wait", json!({}));
    transcript.progress(AgentEvent::ToolFinished {
        id: "wait".into(),
        result: json!({
            "ok": true,
            "background_results": [{
                "task_id": "task-1",
                "status": "exited",
                "exit_code": 0,
                "output": "last\n",
                "pipes": {
                    "run_id": RUN,
                    "uri": format!("pipe://{RUN}"),
                    "pipelines": [{"id": 1, "status": 0, "stages": []}],
                    "nonzero_stages": 1,
                    "incomplete": true,
                },
            }],
        }),
    });
    assert_eq!(
        transcript.entries[0].footer_warning,
        "Non-zero stage results: 1 · stage output incomplete"
    );
    assert!(transcript.entries[0].footer.contains("Task task-1"));
    assert!(!transcript.entries[0].text.contains("/pipes"));
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
        "Timeout: 300s",
        "╰─",
        "Check the diff 中",
    ] {
        assert!(text.contains(expected), "{text}");
    }
    assert!(!text.contains("line-00"));
    assert_eq!(buffer[(0, 0)].fg, bash_border());
    assert_eq!(buffer[(2, 1)].fg, palette::shell_title());
    assert_eq!(buffer[(4, 1)].fg, palette::tool_command());
    assert!(text.contains("Bash · done"));
    for column in 3..7 {
        assert_eq!(buffer[(column, 0)].fg, palette::bash_title());
    }
    assert_eq!(buffer[(2, 1)].bg, bash_background());
    let think_row = text
        .lines()
        .position(|line| line.contains("Check the diff"))
        .unwrap() as u16;
    assert_eq!(buffer[(0, think_row)].symbol(), " ");
    assert_eq!(buffer[(2, think_row)].fg, muted_text());
    assert_eq!(buffer[(2, think_row)].bg, assistant_background());
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
