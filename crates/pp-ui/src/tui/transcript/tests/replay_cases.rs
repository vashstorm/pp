use super::*;

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
    assert!(text.contains("Timeout: 60s"));
    assert!(!text.contains("Exit:"));
    assert!(!text.contains("Wall:"));
    assert_eq!(error.as_deref(), Some("Previous request was interrupted."));
    assert!(!text.contains("Previous request was interrupted."));
    assert_eq!(
        transcript.entries.last().unwrap().status,
        EntryStatus::Failed
    );

    // A fully finished record does not report a false interruption.
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
