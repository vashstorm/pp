use super::*;

#[test]
fn streaming_scrolling_clamp_and_resize_reflow() {
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

    let mut scrolled = Transcript::new();
    let history = (0..60)
        .map(|index| format!("line-{index:02}-abcdefghijk"))
        .collect::<Vec<_>>()
        .join("\n");
    delta(&mut scrolled, &history, false);
    let bottom = screen(&render(&mut scrolled, 12, 8));
    assert!(bottom.contains("line-59"));
    scrolled.scroll_up(3);
    let older = screen(&render(&mut scrolled, 12, 8));
    assert_eq!(
        older.lines().skip(3).collect::<Vec<_>>(),
        bottom.lines().take(5).collect::<Vec<_>>()
    );
    assert!(!older.contains("line-59"));
    delta(&mut scrolled, "\nnew-a\nnew-b", false);
    assert_eq!(screen(&render(&mut scrolled, 12, 8)), older);
    assert!(!scrolled.follow_bottom);

    scrolled.scroll_up(usize::MAX);
    let top = screen(&render(&mut scrolled, 12, 8));
    assert!(top.starts_with("  line-00") && top.contains("line-00"));
    scrolled.scroll_up(3);
    assert_eq!(screen(&render(&mut scrolled, 12, 8)), top);
    scrolled.scroll_down(usize::MAX);
    assert!(screen(&render(&mut scrolled, 12, 8)).contains("new-b"));
    assert!(scrolled.follow_bottom);
    delta(&mut scrolled, "\nfollow", false);
    assert!(screen(&render(&mut scrolled, 12, 8)).contains("follow"));

    let resized = screen(&render(&mut scrolled, 9, 5));
    scrolled.scroll_up(3);
    let older = screen(&render(&mut scrolled, 9, 5));
    assert_eq!(
        older.lines().skip(3).collect::<Vec<_>>(),
        resized.lines().take(2).collect::<Vec<_>>()
    );
    scrolled.scroll_down(usize::MAX);
    assert_eq!(screen(&render(&mut scrolled, 9, 5)), resized);

    for text in ["", "short"] {
        let mut t = Transcript::new();
        if !text.is_empty() {
            delta(&mut t, text, false);
        }
        let original = render(&mut t, 20, 8);
        t.scroll_up(usize::MAX);
        assert_eq!(render(&mut t, 20, 8), original);
        t.scroll_down(usize::MAX);
        assert_eq!(render(&mut t, 20, 8), original);
        assert!(t.follow_bottom);
    }

    let mut reflow = Transcript::new();
    reflow.append("You", "draft submitted 中😀");
    reflow.progress(AgentEvent::AssistantStarted);
    delta(&mut reflow, "first\n  indented 中😀\n\nlast", false);
    let before = render(&mut reflow, 40, 8);
    render(&mut reflow, 5, 2);
    let after = render(&mut reflow, 40, 8);
    assert_eq!(before, after);
    assert_eq!(reflow.entries[1].text, "first\n  indented 中😀\n\nlast");
    reflow.page_up();
    render(&mut reflow, 20, 4);
    assert!(!reflow.follow_bottom);
    assert!(reflow.offset <= reflow.total.saturating_sub(4));
}

#[test]
fn streaming_completion_preserves_exact_markdown_layout_when_visible_text_is_unchanged() {
    let _worker = crate::tui::render_worker::Worker::start().unwrap();
    let text = format!(
        "# Preserved heading\n\n{}\n```rust\nlet value = 1;\n```",
        "**Body** with Unicode 🙂 and `inline code`.\n\n".repeat(200)
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
            assert!(
                Instant::now() < deadline,
                "timed out waiting for streaming Markdown layout"
            );
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
        assert!(
            entry.render.exact,
            "the exact layout must be kept once the same body completes"
        );
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
    assert!(transcript.entries[1].footer.contains("Timeout: 60s"));
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
fn streaming_large_u16_rows_and_eval_card() {
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

    let mut wide = Transcript::new();
    wide.append("Assistant", &format!("{}tail中😀", "a".repeat(140_000)));
    let buffer = render(&mut wide, 3, 8);
    assert!(wide.offset > usize::from(u16::MAX));
    let narrow = screen(&buffer);
    assert!(narrow.contains('t') && narrow.contains('l'));
    let layout = wide.entries[0].layout.as_ref().unwrap();
    assert!(
        layout
            .iter()
            .all(|line| line.height <= usize::from(u16::MAX))
    );
    assert!(screen(&render(&mut wide, 30, 5)).contains("tail中"));

    let mut eval = Transcript::new();
    eval.progress(AgentEvent::ToolStarted {
        id: "eval-1".into(),
        name: "eval".into(),
        arguments: json!({"language":"py","title":"sum check","code":"x = 20\nprint(x + 22)"})
            .to_string(),
    });
    assert_eq!(eval.entries[0].kind, EntryKind::Eval);
    assert!(eval.entries[0].title.contains("Python · sum check"));
    eval.progress(AgentEvent::ToolOutputSnapshot {
        id: "eval-1".into(),
        output: "42\n".into(),
    });
    eval.progress(AgentEvent::ToolFinished {
        id: "eval-1".into(),
        result: json!({"ok":true,"language":"py","output":"42\n","duration_ms":12}),
    });
    let view = screen(&render(&mut eval, 80, 12));
    assert!(
        view.contains("Python · done") && !view.contains("Eval · "),
        "{view}"
    );
    assert!(
        view.contains("sum check") && view.contains("42") && !view.contains("Bash"),
        "{view}"
    );
}
