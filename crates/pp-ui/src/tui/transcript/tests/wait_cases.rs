use super::*;

fn wait_snapshot(count: usize, milliseconds: u64) -> Value {
    json!({"ok":true,"jobs":(0..count).map(|i|json!({"id":format!("bg-{i}"),"type":"bash","label":format!("work-{i}"),"status":"running","duration_ms":milliseconds,"delivered":false})).collect::<Vec<_>>(),"results":[],"background_results":[],"pending":true,"timed_out":false,"interrupted":false})
}

#[test]
fn wait_card_updates_caps_expands_and_freezes_only_final_summary() {
    let mut transcript = Transcript::new();
    transcript.begin_run();
    tool(&mut transcript, "wait", "wait", json!({}));
    let text = screen(&render(&mut transcript, 100, 30));
    assert!(
        text.lines().any(|line| {
            line.starts_with("  │ ") && line.contains("Preparing wait") && line.ends_with('│')
        }),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|line| line.starts_with("  ╭") && line.ends_with('╮')),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|line| line.starts_with("  ╰") && line.ends_with('╯')),
        "{text}"
    );
    for (ms, clock) in [
        (0, "\u{f1456}"),
        (120, "\u{f144b}"),
        (360, "\u{f144d}"),
        (720, "\u{f1450}"),
        (1080, "\u{f1453}"),
        (1320, "\u{f1455}"),
        (1440, "\u{f1456}"),
    ] {
        transcript.progress(AgentEvent::ToolOutputSnapshot {
            id: "wait".into(),
            output: wait_snapshot(10, ms).to_string(),
        });
        let text = entry_text(&mut transcript.entries[0], 100);
        assert!(
            text.contains("10 running") && text.contains("0/10 settled"),
            "{text}"
        );
        assert!(
            text.contains("2 more jobs") && !text.contains("work-9"),
            "{text}"
        );
        assert!(text.contains(&format!("{clock} [bash] bg-0")), "{text}");
        assert!(text.contains("╭") && text.contains("╰"));
    }
    // 较早完成目标的长耗时不能冻结其他运行目标的动画。
    let mut mixed = wait_snapshot(10, 1080);
    mixed["jobs"][0]["status"] = json!("completed");
    mixed["jobs"][0]["duration_ms"] = json!(10_000_000);
    transcript.progress(AgentEvent::ToolOutputSnapshot {
        id: "wait".into(),
        output: mixed.to_string(),
    });
    let text = entry_text(&mut transcript.entries[0], 100);
    assert!(
        text.contains("\u{f00c} [bash] bg-0") && text.contains("\u{f1453} [bash] bg-1"),
        "{text}"
    );
    transcript.toggle_tool_output();
    assert!(entry_text(&mut transcript.entries[0], 100).contains("work-9"));
    let mut result = wait_snapshot(10, 2000);
    for job in result["jobs"].as_array_mut().unwrap() {
        job["status"] = json!("exited");
    }
    result["pending"] = json!(false);
    result["jobs"][1]["type"] = json!("task");
    result["jobs"][1]["status"] = json!("completed");
    result["results"] = json!([{"run":{"id":"bg-1","result":{"output":"agent-report","error":"agent-error","output_path":"/tmp/agent-result.json"}}}]);
    result["background_results"] =
        json!([{"task_id":"bg-0","status":"exited","ok":true,"output":"finished-line"}]);
    transcript.progress(AgentEvent::ToolFinished {
        id: "wait".into(),
        result: result.clone(),
    });
    let frozen = entry_text(&mut transcript.entries[0], 100);
    assert!(
        frozen.contains("10/10 settled") && frozen.contains("0 running"),
        "{frozen}"
    );
    assert!(!frozen.contains("Result ") && !frozen.contains("finished-line"));
    assert!(!frozen.contains("agent-report") && !frozen.contains("agent-error"));
    assert!(!frozen.contains("/tmp/agent-result.json"));
    assert!(!frozen.contains(palette::WAITING_CLOCK_FRAMES));
    transcript.tick(Instant::now() + Duration::from_secs(20));
    assert_eq!(entry_text(&mut transcript.entries[0], 100), frozen);
    let replay = vec![
        ReplayRecord::Event(AgentEvent::ToolStarted {
            id: "wait".into(),
            name: "wait".into(),
            arguments: "{}".into(),
        }),
        ReplayRecord::Event(AgentEvent::ToolFinished {
            id: "wait".into(),
            result,
        }),
    ];
    let mut restored = Transcript::new();
    restored.replay(replay);
    let text = entry_text(&mut restored.entries[0], 100);
    assert!(text.contains("10/10 settled"), "{text}");
    assert!(!text.contains("Result ") && !text.contains("finished-line"));
    assert!(!text.contains("agent-report") && !text.contains("agent-error"));
    assert!(!text.contains("/tmp/agent-result.json"));
    restored.toggle_tool_output();
    assert_eq!(entry_text(&mut restored.entries[0], 100), frozen);
}

#[test]
fn wait_card_updates_background_original_and_marks_timeout_and_interruption() {
    let mut transcript = Transcript::new();
    transcript.begin_run();
    tool(
        &mut transcript,
        "bg",
        "bash",
        json!({"command":"run-work","mode":"background"}),
    );
    transcript.progress(AgentEvent::ToolFinished {
        id: "bg".into(),
        result: json!({"ok":true,"task_id":"bg-0","status":"running","output":"start"}),
    });
    tool(&mut transcript, "wait", "wait", json!({}));
    let mut result = wait_snapshot(1, 1000);
    result["jobs"][0]["status"] = json!("failed");
    result["background_results"] = json!([{"ok":false,"task_id":"bg-0","status":"failed","exit_code":7,"output":"final-output","error":"failed command"}]);
    transcript.progress(AgentEvent::ToolFinished {
        id: "wait".into(),
        result,
    });
    assert_eq!(transcript.entries[0].status, EntryStatus::Failed);
    assert!(entry_text(&mut transcript.entries[0], 100).contains("final-output"));
    let text = entry_text(&mut transcript.entries[1], 100);
    assert!(
        text.contains("1 failed")
            && text.contains("1/1 settled")
            && text.contains("\u{f00d} [bash] bg-0"),
        "{text}"
    );
    assert!(!text.contains("Result ") && !text.contains("final-output"));
    assert!(!text.contains("failed command"));
    for (field, notice) in [
        ("timed_out", "timed out"),
        ("interrupted", "interrupted by steering"),
    ] {
        tool(&mut transcript, field, "wait", json!({}));
        let mut result = wait_snapshot(1, 1000);
        result[field] = json!(true);
        transcript.progress(AgentEvent::ToolFinished {
            id: field.into(),
            result,
        });
        let last = transcript.entries.last_mut().unwrap();
        assert!(entry_text(last, 100).contains(notice));
        for width in [1, 7, 8, 24] {
            last.layout = None;
            last.ensure_layout(width);
            for line in &last.layout.as_ref().unwrap().lines {
                assert!(last.line(line).width() <= usize::from(width));
            }
        }
    }
}

#[test]
fn wait_card_native_history_never_freezes_partial_running_state() {
    let mut transcript = Transcript::new();
    transcript.begin_run();
    tool(&mut transcript, "wait", "wait", json!({}));
    let mut result = wait_snapshot(3, 1000);
    for settled in 0..=3 {
        for job in result["jobs"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .take(settled)
        {
            job["status"] = json!("exited");
        }
        transcript.progress(AgentEvent::ToolOutputSnapshot {
            id: "wait".into(),
            output: result.to_string(),
        });
        assert_eq!(transcript.entries.len(), 1);
        assert!(transcript.next_native_batch(100, 1, false).is_none());
    }
    result["pending"] = json!(false);
    transcript.progress(AgentEvent::ToolFinished {
        id: "wait".into(),
        result,
    });
    let history = retire_native(&mut transcript, 100, 1, false);
    assert_eq!(history.matches("Wait ·").count(), 1, "{history}");
    assert!(
        history.contains("0 running") && history.contains("3/3 settled"),
        "{history}"
    );
    assert!(!history.contains("· running ·"), "{history}");
}
