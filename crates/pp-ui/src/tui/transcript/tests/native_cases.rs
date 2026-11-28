use super::*;

#[test]
fn native_history_requires_ack_and_keeps_only_bounded_frozen_tail() {
    let mut transcript = Transcript::new();
    transcript.append(
        "You",
        &(0..200)
            .map(|i| format!("native-{i:03}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let batch = transcript.next_native_batch(40, 10, false).unwrap();
    let repeated = transcript.next_native_batch(40, 10, false).unwrap();
    assert_eq!(native_rows(&batch.rows), native_rows(&repeated.rows));
    assert_eq!(transcript.native.entry, 0);
    assert_eq!(transcript.native.row, 0);
    let mut text = native_rows(&batch.rows);
    transcript.acknowledge_native_batch(batch);
    text.push_str(&retire_native(&mut transcript, 40, 10, false));
    assert_eq!(text.matches("native-000").count(), 1);
    assert_eq!(text.matches("native-100").count(), 1);
    assert_eq!(transcript.native.entry, 1);
    assert!(transcript.native.retained.len() <= 10);
    assert!(
        retire_native(&mut transcript, 40, 10, true)
            .trim()
            .is_empty()
    );
    // The full data is still available to the alternate screen.
    assert!(screen(&render(&mut transcript, 40, 230)).contains("native-000"));
}

#[test]
fn native_history_defers_streaming_and_seals_partial_output_only_on_exit() {
    let mut transcript = Transcript::new();
    transcript.begin_run();
    transcript.progress(AgentEvent::AssistantStarted);
    delta(
        &mut transcript,
        &(0..80)
            .map(|i| format!("partial-{i:03}\n\n"))
            .collect::<String>(),
        false,
    );
    assert!(transcript.next_native_batch(40, 8, false).is_none());
    let output = retire_native(&mut transcript, 40, 8, true);
    assert!(output.contains("partial-000"));
    assert_eq!(output.matches("partial-000").count(), 1);
    assert!(
        retire_native(&mut transcript, 40, 8, true)
            .trim()
            .is_empty()
    );
}

/// A frame may only report "nothing to commit" once the frozen rows fit on screen;
/// otherwise rows that already left the viewport would stay off the terminal.
#[test]
fn native_history_emits_every_frozen_row_once_in_order() {
    let mut transcript = Transcript::new();
    transcript.append(
        "You",
        &(0..300)
            .map(|i| format!("overflow-{i:03}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let (width, height) = (40u16, 10u16);
    let mut batches = 0usize;
    let mut markers = Vec::new();
    while let Some(batch) = transcript.next_native_batch(width, height, false) {
        batches += 1;
        markers.extend(
            native_rows(&batch.rows)
                .lines()
                .filter_map(|line| {
                    line.find("overflow-").map(|start| {
                        line[start..]
                            .chars()
                            .take("overflow-000".len())
                            .collect::<String>()
                    })
                })
                .collect::<Vec<_>>(),
        );
        transcript.acknowledge_native_batch(batch);
    }
    assert!(batches > 1, "the fixture must span several batches");
    let expected = (0..300)
        .map(|index| format!("overflow-{index:03}"))
        .collect::<Vec<_>>();
    assert_eq!(
        markers,
        expected[..markers.len()],
        "frozen rows must reach the terminal once and in order"
    );
    assert!(
        expected.len() - markers.len() <= usize::from(height),
        "only rows that still fit on screen may stay uncommitted: {} of {} rows emitted",
        markers.len(),
        expected.len()
    );
}

#[test]
fn native_history_freezes_tool_preview_and_waits_for_read_group_boundary() {
    let mut transcript = Transcript::with_read_preview(false);
    transcript.begin_run();
    tool(&mut transcript, "read-a", "read", json!({"path":"a.rs"}));
    transcript.progress(AgentEvent::ToolFinished {
        id: "read-a".into(),
        result: json!({"ok":true,"content":"READ_HIDDEN"}),
    });
    assert!(transcript.next_native_batch(40, 1, false).is_none());
    transcript.append("Assistant", "read group closed");
    let output = retire_native(&mut transcript, 40, 1, false);
    assert!(output.contains("a.rs"));
    assert!(!output.contains("READ_HIDDEN"));
    transcript.toggle_tool_output();
    assert!(
        retire_native(&mut transcript, 24, 1, true)
            .trim()
            .is_empty()
    );
    assert_eq!(
        transcript.entries[0].result.as_ref().unwrap()["content"],
        "READ_HIDDEN"
    );
}
