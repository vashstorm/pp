use super::*;
use crate::tui::render_worker::Worker;

fn wait_exact(transcript: &mut Transcript, width: u16) -> Buffer {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let buffer = render(transcript, width, 8);
        if transcript.entries.iter().all(|entry| {
            !matches!(entry.kind, EntryKind::Assistant | EntryKind::Think) || entry.render.exact
        }) {
            return buffer;
        }
        assert!(Instant::now() < deadline, "后台排版应完成");
        std::thread::yield_now();
    }
}

#[test]
fn streaming_scroll_keeps_markdown_snapshot_and_long_position_while_pending() {
    let worker = Worker::start().unwrap();
    for thinking in [false, true] {
        for long in [false, true] {
            let mut transcript = Transcript::new();
            transcript.progress(AgentEvent::AssistantStarted);
            let text = if long {
                format!(
                    "```text\n{}",
                    (0..1200)
                        .map(|i| format!("line-{i:04} 中🙂\n"))
                        .collect::<String>()
                )
            } else {
                (0..60)
                    .map(|i| format!("- **line-{i:04}** `value`\n"))
                    .collect()
            };
            let event = |text: String| {
                if thinking {
                    AgentEvent::ThinkingDelta { text }
                } else {
                    AgentEvent::AssistantDelta {
                        text,
                        refusal: false,
                    }
                }
            };
            transcript.progress(event(text));
            wait_exact(&mut transcript, 60);
            transcript.scroll_up(12);
            let before = render(&mut transcript, 60, 8);
            let offset = transcript.offset;
            let pause = worker.pause();
            transcript.progress(event("new-tail\n".into()));
            assert!(
                !transcript.entries[0].render.exact,
                "新分片立即撤销精确资格"
            );
            let pending = render(&mut transcript, 60, 8);
            pause.wait();
            assert_eq!(before, pending, "等待后台时正文和样式应稳定");
            assert_eq!(transcript.offset, offset);
            assert!(
                transcript.next_native_batch(60, 8, true).is_none(),
                "旧快照不能提交原生历史"
            );
            drop(pause);
            assert_eq!(wait_exact(&mut transcript, 60), before);
            assert_eq!(transcript.offset, offset);
            transcript.scroll_down(usize::MAX);
            assert!(transcript.following());
            assert!(screen(&render(&mut transcript, 60, 8)).contains("new-tail"));
        }
    }
}

#[test]
fn streaming_scroll_anchors_entry_when_preceding_height_changes() {
    let mut transcript = Transcript::new();
    transcript.append("Note", "old\n".repeat(20).as_str());
    transcript.append(
        "Assistant",
        &(0..60)
            .map(|i| format!("line-{i:03}\n\n"))
            .collect::<String>(),
    );
    render(&mut transcript, 60, 8);
    transcript.scroll_up(12);
    let before = render(&mut transcript, 60, 8);
    let offset = transcript.offset;
    transcript.entries[0].text.push_str(&"extra\n".repeat(30));
    transcript.entries[0].layout = None;
    assert_eq!(render(&mut transcript, 60, 8), before);
    assert_eq!(transcript.offset, offset + 30);
    transcript.scroll_up(3);
    assert_ne!(render(&mut transcript, 60, 8), before);
}

#[test]
fn streaming_scroll_restores_requested_row_after_temporary_resize_preview() {
    let worker = Worker::start().unwrap();
    let mut transcript = Transcript::new();
    transcript.progress(AgentEvent::AssistantStarted);
    delta(
        &mut transcript,
        &format!(
            "```text\n{}",
            (0..1200)
                .map(|i| format!("line-{i:04} content\n"))
                .collect::<String>()
        ),
        false,
    );
    wait_exact(&mut transcript, 60);
    transcript.scroll_up(12);
    let before = render(&mut transcript, 60, 8);
    let marker = screen(&before).lines().next().unwrap().trim().to_owned();
    let offset = transcript.offset;
    let pause = worker.pause();
    transcript.resize_preserving_anchor(40);
    render(&mut transcript, 40, 8);
    pause.wait();
    drop(pause);
    let after = wait_exact(&mut transcript, 40);
    assert_eq!(screen(&after).lines().next().unwrap().trim(), marker);
    assert_eq!(transcript.offset, offset);
    assert!(!transcript.following());
}

#[test]
fn streaming_scroll_completed_old_snapshot_waits_for_latest_before_exit_commit() {
    let worker = Worker::start().unwrap();
    let mut transcript = Transcript::new();
    transcript.progress(AgentEvent::AssistantStarted);
    let text = "- **stable** `value`\n".repeat(60);
    delta(&mut transcript, &text, false);
    wait_exact(&mut transcript, 60);
    let pause = worker.pause();
    delta(&mut transcript, "- **new-tail**\n", false);
    transcript.progress(AgentEvent::AssistantFinished {
        text: format!("{text}- **new-tail**\n"),
    });
    render(&mut transcript, 60, 8);
    pause.wait();
    assert!(transcript.next_native_batch(60, 8, true).is_none());
    drop(pause);
    wait_exact(&mut transcript, 60);
    let mut rows = retire_native(&mut transcript, 60, 8, true);
    rows.push_str(&native_rows(transcript.native.retained.make_contiguous()));
    assert!(rows.contains("new-tail"));
    assert!(!rows.contains("**"));
}

#[test]
fn streaming_scroll_static_history_survives_live_output_cancel_and_reset() {
    let worker = Worker::start().unwrap();
    let mut transcript = Transcript::new();
    transcript.append(
        "Note",
        &(0..100)
            .map(|i| format!("static-{i:03}\n"))
            .collect::<String>(),
    );
    transcript.progress(AgentEvent::AssistantStarted);
    delta(&mut transcript, &"- **live** `value`\n".repeat(1000), false);
    wait_exact(&mut transcript, 60);
    transcript.scroll_up(usize::MAX);
    let before = render(&mut transcript, 60, 8);
    let pause = worker.pause();
    delta(&mut transcript, "new-tail", false);
    assert_eq!(render(&mut transcript, 60, 8), before);
    pause.wait();
    transcript.cancel();
    assert_eq!(render(&mut transcript, 60, 8), before);
    drop(pause);
    assert_eq!(wait_exact(&mut transcript, 60), before);
    transcript = transcript.fresh();
    assert!(transcript.browse_anchor.is_none());
    assert!(transcript.displayed_heights.is_empty());
    assert!(transcript.following());
    assert!(!screen(&render(&mut transcript, 60, 8)).contains("static-"));
}

#[test]
fn streaming_scroll_clamps_replaced_body_only_after_exact_layout() {
    let worker = Worker::start().unwrap();
    let mut transcript = Transcript::new();
    transcript.progress(AgentEvent::AssistantStarted);
    delta(&mut transcript, &"- **old**\n".repeat(100), false);
    wait_exact(&mut transcript, 60);
    transcript.scroll_up(12);
    render(&mut transcript, 60, 8);
    let requested = transcript.browse_anchor.unwrap().row;
    let pause = worker.pause();
    transcript.progress(AgentEvent::AssistantFinished {
        text: "**replacement**".into(),
    });
    let preview = render(&mut transcript, 60, 8);
    pause.wait();
    assert!(!screen(&preview).contains("old"));
    assert_eq!(transcript.browse_anchor.unwrap().row, requested);
    drop(pause);
    wait_exact(&mut transcript, 60);
    assert_eq!(transcript.browse_anchor.unwrap().row, 1);
    transcript.append("Note", "more history\n".repeat(100).as_str());
    render(&mut transcript, 60, 8);
    assert_eq!(transcript.offset, 1);
    assert!(!transcript.following());
}
