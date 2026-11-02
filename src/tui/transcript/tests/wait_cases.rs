use super::*;

fn wait_snapshot(count: usize, milliseconds: u64) -> Value {
    json!({"ok":true,"jobs":(0..count).map(|i|json!({"id":format!("bg-{i}"),"type":"bash","label":format!("work-{i}"),"status":"running","duration_ms":milliseconds,"delivered":false})).collect::<Vec<_>>(),"results":[],"background_results":[],"pending":true,"timed_out":false,"interrupted":false})
}

#[test]
fn wait_card_updates_caps_expands_and_freezes_after_result() {
    let mut transcript = Transcript::new();
    transcript.begin_run();
    tool(&mut transcript, "wait", "wait", json!({}));
    assert!(screen(&render(&mut transcript, 100, 30)).contains("Preparing wait"));
    for ms in [1000, 1500] {
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
        assert!(text.contains(if ms == 1000 { "◑" } else { "◒" }), "{text}");
        assert!(text.contains("╭") && text.contains("╰"));
    }
    // 较早完成目标的长耗时不能冻结其他运行目标的动画。
    let mut mixed = wait_snapshot(10, 1500);
    mixed["jobs"][0]["status"] = json!("completed");
    mixed["jobs"][0]["duration_ms"] = json!(10_000_000);
    transcript.progress(AgentEvent::ToolOutputSnapshot {
        id: "wait".into(),
        output: mixed.to_string(),
    });
    assert!(entry_text(&mut transcript.entries[0], 100).contains("◒"));
    transcript.toggle_tool_output();
    assert!(entry_text(&mut transcript.entries[0], 100).contains("work-9"));
    let mut result = wait_snapshot(10, 2000);
    result["jobs"][0]["status"] = json!("exited");
    result["background_results"] =
        json!([{"task_id":"bg-0","status":"exited","ok":true,"output":"finished-line"}]);
    transcript.progress(AgentEvent::ToolFinished {
        id: "wait".into(),
        result: result.clone(),
    });
    let frozen = entry_text(&mut transcript.entries[0], 100);
    assert!(
        frozen.contains("1/10 settled")
            && frozen.contains("9 running")
            && frozen.contains("finished-line"),
        "{frozen}"
    );
    assert!(!frozen.contains("◑") && !frozen.contains("◒"));
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
    assert!(
        text.contains("1/10 settled") && text.contains("finished-line"),
        "{text}"
    );
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
        text.contains("1 failed") && text.contains("1/1 settled"),
        "{text}"
    );
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
