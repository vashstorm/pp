use super::*;

fn snapshot(transcript: &mut Transcript, call: &str, task: &str, status: &str, output: &str) {
    transcript.progress(AgentEvent::ToolOutputSnapshot {
        id: call.into(),
        output: output.into(),
    });
    transcript.progress(AgentEvent::ToolFinished {
        id: call.into(),
        result: json!({"ok":true,"task_id":task,"status":status,"output":output,"exit_code":0}),
    });
}

#[test]
fn bash_task_polls_update_original_card_without_duplicate_output() {
    let mut transcript = Transcript::new();
    transcript.begin_run();
    tool(
        &mut transcript,
        "start",
        "bash",
        json!({"command":"run-server","mode":"background"}),
    );
    snapshot(&mut transcript, "start", "bg-1", "running", "first\n");
    let text = screen(&render(&mut transcript, 100, 30));
    assert!(text.contains("Bash · running"), "{text}");
    transcript.append("Assistant", "继续检查服务");
    transcript.toggle_tool_output();
    for poll in 0..5 {
        let id = format!("poll-{poll}");
        tool(
            &mut transcript,
            &id,
            "bash_task",
            json!({"action":"status","task_id":"bg-1"}),
        );
        assert_eq!(transcript.entries.len(), 2);
        assert_eq!(transcript.tool_index(&id), Some(0));
        assert!(
            !transcript.entries[0].text.is_empty(),
            "查询中保留上一份输出"
        );
        let output = format!("first\nlatest-{poll}\n");
        transcript.progress(AgentEvent::ToolOutputSnapshot {
            id: id.clone(),
            output: output.clone(),
        });
        let text = screen(&render(&mut transcript, 100, 30));
        assert_eq!(text.matches("first").count(), 1, "{text}");
        snapshot(&mut transcript, &id, "bg-1", "running", &output);
        assert_eq!(transcript.entries[0].text, output.trim_end());
        assert!(transcript.entries[0].expanded);
        let text = screen(&render(&mut transcript, 100, 30));
        assert_eq!(text.matches("first").count(), 1, "{text}");
        assert_eq!(text.matches("Bash · running").count(), 1, "{text}");
        assert!(text.contains("$ run-server"));
    }
    tool(
        &mut transcript,
        "stop",
        "bash_task",
        json!({"action":"stop","task_id":"bg-1"}),
    );
    snapshot(&mut transcript, "stop", "bg-1", "stopped", "first\nlast\n");
    assert_eq!(transcript.entries.len(), 2);
    let text = screen(&render(&mut transcript, 100, 30));
    assert!(
        text.contains("Bash · done") && text.contains("Task bg-1: stopped"),
        "{text}"
    );
    assert!(!text.contains("latest-4"));
}

#[test]
fn bash_task_failure_preserves_logs_and_next_snapshot_recovers_in_place() {
    let mut transcript = Transcript::new();
    tool(
        &mut transcript,
        "first",
        "bash_task",
        json!({"action":"status","task_id":"bg-1"}),
    );
    snapshot(&mut transcript, "first", "bg-1", "running", "saved-log\n");
    tool(
        &mut transcript,
        "error",
        "bash_task",
        json!({"action":"status","task_id":"bg-1"}),
    );
    transcript.progress(AgentEvent::ToolFinished {
        id: "error".into(),
        result: json!({"ok":false,"error":"query failed"}),
    });
    let buffer = render(&mut transcript, 100, 30);
    let text = screen(&buffer);
    assert_eq!(transcript.entries.len(), 1);
    assert!(
        text.contains("saved-log") && text.contains("query failed"),
        "{text}"
    );
    assert_eq!(buffer[(0, 0)].fg, palette::REMOVED);
    tool(
        &mut transcript,
        "retry",
        "bash_task",
        json!({"action":"status","task_id":"bg-1"}),
    );
    snapshot(
        &mut transcript,
        "retry",
        "bg-1",
        "exited",
        "saved-log\nfinished\n",
    );
    let text = screen(&render(&mut transcript, 100, 30));
    assert_eq!(transcript.entries.len(), 1);
    assert!(
        text.contains("finished") && !text.contains("query failed"),
        "{text}"
    );
}

#[test]
fn bash_task_keeps_distinct_processes_foreground_and_new_runs_separate() {
    let mut transcript = Transcript::new();
    for task in ["bg-1", "bg-2"] {
        tool(
            &mut transcript,
            task,
            "bash",
            json!({"command":"same-command","mode":"background"}),
        );
        snapshot(&mut transcript, task, task, "running", task);
    }
    tool(
        &mut transcript,
        "fg",
        "bash",
        json!({"command":"same-command"}),
    );
    snapshot(&mut transcript, "fg", "ignored", "exited", "foreground");
    assert_eq!(transcript.background_bash.len(), 2);
    tool(
        &mut transcript,
        "poll",
        "bash_task",
        json!({"action":"status","task_id":"bg-2"}),
    );
    snapshot(&mut transcript, "poll", "bg-2", "running", "updated-second");
    assert_eq!(transcript.entries.len(), 3);
    assert_eq!(transcript.entries[0].text, "bg-1");
    assert_eq!(transcript.entries[1].text, "updated-second");
    transcript.finish_run_with_timing("Completed", None, None);
    transcript.begin_run();
    tool(
        &mut transcript,
        "new-poll",
        "bash_task",
        json!({"action":"status","task_id":"bg-2"}),
    );
    assert_eq!(transcript.entries.len(), 4);
}

#[test]
fn bash_task_native_history_waits_for_run_end_and_commits_final_card_once() {
    let mut transcript = Transcript::new();
    transcript.begin_run();
    tool(
        &mut transcript,
        "start",
        "bash",
        json!({"command":"native-command","mode":"background"}),
    );
    snapshot(&mut transcript, "start", "bg-1", "exited", "old-log");
    transcript.append("Assistant", &"middle\n".repeat(80));
    assert!(transcript.next_native_batch(100, 10, false).is_none());
    tool(
        &mut transcript,
        "poll",
        "bash_task",
        json!({"action":"status","task_id":"bg-1"}),
    );
    snapshot(&mut transcript, "poll", "bg-1", "exited", "final-log");
    assert!(transcript.next_native_batch(100, 10, false).is_none());
    transcript.finish_run_with_timing("Completed", None, None);
    let text = retire_native(&mut transcript, 100, 10, false);
    assert_eq!(text.matches("native-command").count(), 1, "{text}");
    assert_eq!(text.matches("final-log").count(), 1, "{text}");
    assert!(!text.contains("old-log") && !text.contains("bash_task"));
    assert!(
        retire_native(&mut transcript, 100, 10, true)
            .trim()
            .is_empty()
    );
}

#[test]
fn bash_task_replay_and_recorded_pages_reuse_card_without_restarting_timer() {
    let start = AgentEvent::ToolStarted {
        id: "start".into(),
        name: "bash".into(),
        arguments: json!({"command":"recorded-command","mode":"background"}).to_string(),
    };
    let started = AgentEvent::ToolFinished {
        id: "start".into(),
        result: json!({"ok":true,"task_id":"bg-1","status":"running","output":"old"}),
    };
    let poll = AgentEvent::ToolStarted {
        id: "poll".into(),
        name: "bash_task".into(),
        arguments: json!({"action":"status","task_id":"bg-1"}).to_string(),
    };
    let finished = AgentEvent::ToolFinished {
        id: "poll".into(),
        result: json!({"ok":true,"task_id":"bg-1","status":"exited","output":"new"}),
    };
    let events = [start, started, poll, finished];
    let mut replay = Transcript::new();
    assert!(
        replay
            .replay(events.iter().cloned().map(ReplayRecord::Event).collect())
            .is_none()
    );
    assert_eq!(replay.entries.len(), 1);
    assert_eq!(replay.entries[0].text, "new");
    let mut recorded = Transcript::new();
    let now = Instant::now();
    for (index, event) in events.into_iter().enumerate() {
        recorded.progress_recorded(event, Some(1000 + index as u64 * 1000), 5000, now);
    }
    assert_eq!(recorded.entries.len(), 1);
    assert_eq!(
        recorded.entries[0].started,
        now.checked_sub(Duration::from_secs(4))
    );
    assert_eq!(recorded.entries[0].text, "new");
    recorded.stop_recorded_run(false);
    assert!(recorded.background_bash.is_empty());
}
