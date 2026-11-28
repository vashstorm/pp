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
        assert!(Instant::now() < deadline, "background layout must finish");
        std::thread::yield_now();
    }
}

#[test]
fn streaming_table_keeps_viewport_stable_until_row_is_complete() {
    let worker = Worker::start().unwrap();
    for thinking in [false, true] {
        let mut transcript = Transcript::new();
        for index in 0..5 {
            transcript.append("You", &format!("Earlier message {index}"));
        }
        let event = |text: &str| {
            if thinking {
                AgentEvent::ThinkingDelta { text: text.into() }
            } else {
                AgentEvent::AssistantDelta {
                    text: text.into(),
                    refusal: false,
                }
            }
        };
        let width = 25;
        let mut source =
            "| 左 | Right column |\n| --- | --- |\n| old | wide content |\n".to_owned();
        transcript.progress(event(&source));
        let initial = wait_exact(&mut transcript, width);
        assert!(screen(&initial).contains("wide content"));
        let preview_offset = transcript.offset + 1;
        let initial_height = transcript.total;
        let initial_screen = screen(&initial);
        let stable_rows = initial_screen.lines().skip(1).collect::<Vec<_>>();

        for (chunk, visible) in [
            ("|", "|"),
            (" 新 row with **bold", "**bold"),
            (" and-more-words", "and-more-words"),
            ("** | 🙂 |", "🙂"),
        ] {
            let pause = worker.pause();
            transcript.progress(event(chunk));
            source.push_str(chunk);
            let pending = render(&mut transcript, width, 8);
            pause.wait();
            let pending_text = screen(&pending);
            let mut rows = pending_text.lines();
            assert_eq!((&mut rows).take(7).collect::<Vec<_>>(), stable_rows);
            assert!(rows.next().unwrap().contains(visible), "{pending_text}");
            assert_eq!(transcript.offset, preview_offset);
            assert_eq!(transcript.total, initial_height + 1);
            drop(pause);
            assert_eq!(
                wait_exact(&mut transcript, width),
                pending,
                "the same half-finished line must look identical before and after background layout"
            );
        }

        let before_newline = wait_exact(&mut transcript, width);
        let pause = worker.pause();
        transcript.progress(event("\n"));
        source.push('\n');
        let pending = render(&mut transcript, width, 8);
        pause.wait();
        assert_eq!(pending, before_newline);
        drop(pause);
        let complete_row = wait_exact(&mut transcript, width);
        assert!(screen(&complete_row).contains("🙂"));
        assert!(!screen(&complete_row).contains("**"));
        let last_row = "| EOF | final |";
        transcript.progress(event(last_row));
        source.push_str(last_row);
        assert!(screen(&wait_exact(&mut transcript, width)).contains(last_row));
        if thinking {
            transcript.cancel();
        } else {
            transcript.progress(AgentEvent::AssistantFinished {
                text: source.clone(),
            });
        }
        let final_screen = screen(&wait_exact(&mut transcript, width));
        assert!(final_screen.contains("EOF"));
        assert!(!final_screen.contains(last_row));
        assert_eq!(transcript.entries.last().unwrap().text, source);
    }
}

#[test]
fn streaming_scroll_live_tail_anchors_and_resize() {
    let worker = Worker::start().unwrap();
    for thinking in [false, true] {
        let mut transcript = Transcript::new();
        transcript.progress(AgentEvent::AssistantStarted);
        let event = |text: &str| {
            if thinking {
                AgentEvent::ThinkingDelta { text: text.into() }
            } else {
                AgentEvent::AssistantDelta {
                    text: text.into(),
                    refusal: false,
                }
            }
        };
        transcript.progress(event("**stable prefix**\n\n"));
        wait_exact(&mut transcript, 60);
        let pause = worker.pause();
        transcript.progress(event("live-tail-中"));
        let first = render(&mut transcript, 60, 8);
        pause.wait();
        assert!(screen(&first).contains("live-tail-中"));
        transcript.progress(event("🙂-newest"));
        let second = render(&mut transcript, 60, 8);
        assert!(screen(&second).contains("live-tail-中"));
        assert!(screen(&second).contains("🙂"));
        assert!(screen(&second).contains("-newest"));
        assert!(!screen(&second).contains("**stable prefix**"));
        assert!(!transcript.entries[0].render.exact);
        assert!(transcript.next_native_batch(60, 8, true).is_none());
        drop(pause);
        assert!(screen(&wait_exact(&mut transcript, 60)).contains("-newest"));

        for long in [false, true] {
            let mut anchored = Transcript::new();
            anchored.progress(AgentEvent::AssistantStarted);
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
            anchored.progress(event(&text));
            wait_exact(&mut anchored, 60);
            anchored.scroll_up(12);
            let before = render(&mut anchored, 60, 8);
            let offset = anchored.offset;
            let pause = worker.pause();
            anchored.progress(event("new-tail\n"));
            assert!(!anchored.entries[0].render.exact);
            let pending = render(&mut anchored, 60, 8);
            pause.wait();
            assert_eq!(before, pending);
            assert_eq!(anchored.offset, offset);
            assert!(anchored.next_native_batch(60, 8, true).is_none());
            drop(pause);
            assert_eq!(wait_exact(&mut anchored, 60), before);
            assert_eq!(anchored.offset, offset);
            anchored.scroll_down(usize::MAX);
            assert!(anchored.following());
            assert!(screen(&render(&mut anchored, 60, 8)).contains("new-tail"));
        }
    }

    let mut height_shifted = Transcript::new();
    height_shifted.append("Note", "old\n".repeat(20).as_str());
    height_shifted.append(
        "Assistant",
        &(0..60)
            .map(|i| format!("line-{i:03}\n\n"))
            .collect::<String>(),
    );
    render(&mut height_shifted, 60, 8);
    height_shifted.scroll_up(12);
    let before = render(&mut height_shifted, 60, 8);
    let offset = height_shifted.offset;
    height_shifted.entries[0]
        .text
        .push_str(&"extra\n".repeat(30));
    height_shifted.entries[0].layout = None;
    assert_eq!(render(&mut height_shifted, 60, 8), before);
    assert_eq!(height_shifted.offset, offset + 30);
    height_shifted.scroll_up(3);
    assert_ne!(render(&mut height_shifted, 60, 8), before);

    let mut resized = Transcript::new();
    resized.progress(AgentEvent::AssistantStarted);
    delta(
        &mut resized,
        &format!(
            "```text\n{}",
            (0..1200)
                .map(|i| format!("line-{i:04} content\n"))
                .collect::<String>()
        ),
        false,
    );
    wait_exact(&mut resized, 60);
    resized.scroll_up(12);
    let before = render(&mut resized, 60, 8);
    let marker = screen(&before).lines().next().unwrap().trim().to_owned();
    let offset = resized.offset;
    let pause = worker.pause();
    resized.resize_preserving_anchor(40);
    render(&mut resized, 40, 8);
    pause.wait();
    drop(pause);
    let after = wait_exact(&mut resized, 40);
    assert_eq!(screen(&after).lines().next().unwrap().trim(), marker);
    assert_eq!(resized.offset, offset);
    assert!(!resized.following());
}

#[test]
fn streaming_scroll_commit_cancel_and_clamping() {
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

    let mut cancelled = Transcript::new();
    cancelled.append(
        "Note",
        &(0..100)
            .map(|i| format!("static-{i:03}\n"))
            .collect::<String>(),
    );
    cancelled.progress(AgentEvent::AssistantStarted);
    delta(&mut cancelled, &"- **live** `value`\n".repeat(1000), false);
    wait_exact(&mut cancelled, 60);
    cancelled.scroll_up(usize::MAX);
    let before = render(&mut cancelled, 60, 8);
    let pause = worker.pause();
    delta(&mut cancelled, "new-tail", false);
    assert_eq!(render(&mut cancelled, 60, 8), before);
    pause.wait();
    cancelled.cancel();
    assert_eq!(render(&mut cancelled, 60, 8), before);
    drop(pause);
    assert_eq!(wait_exact(&mut cancelled, 60), before);
    cancelled = cancelled.fresh();
    assert!(cancelled.browse_anchor.is_none());
    assert!(cancelled.geometry.is_empty());
    assert!(cancelled.following());
    assert!(!screen(&render(&mut cancelled, 60, 8)).contains("static-"));

    let mut clamped = Transcript::new();
    clamped.progress(AgentEvent::AssistantStarted);
    delta(&mut clamped, &"- **old**\n".repeat(100), false);
    wait_exact(&mut clamped, 60);
    clamped.scroll_up(12);
    render(&mut clamped, 60, 8);
    let requested = clamped.browse_anchor.unwrap().row;
    let pause = worker.pause();
    clamped.progress(AgentEvent::AssistantFinished {
        text: "**replacement**".into(),
    });
    let preview = render(&mut clamped, 60, 8);
    pause.wait();
    assert!(!screen(&preview).contains("old"));
    assert_eq!(clamped.browse_anchor.unwrap().row, requested);
    drop(pause);
    wait_exact(&mut clamped, 60);
    assert_eq!(clamped.browse_anchor.unwrap().row, 1);
    clamped.append("Note", "more history\n".repeat(100).as_str());
    render(&mut clamped, 60, 8);
    assert_eq!(clamped.offset, 1);
    assert!(!clamped.following());
}

fn render_native(transcript: &mut Transcript, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| transcript.draw_native(frame, frame.area()))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn wait_exact_native(transcript: &mut Transcript, width: u16, height: u16) -> Buffer {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let buffer = render_native(transcript, width, height);
        if transcript.entries.iter().all(|entry| {
            !matches!(entry.kind, EntryKind::Assistant | EntryKind::Think) || entry.render.exact
        }) {
            return buffer;
        }
        assert!(Instant::now() < deadline, "background layout must finish");
        std::thread::yield_now();
    }
}

#[test]
fn streaming_think_and_assistant_preview_matches_exact_without_jumping_upper_rows() {
    let worker = Worker::start().unwrap();
    for thinking in [true, false] {
        let mut transcript = Transcript::new();
        transcript.progress(AgentEvent::AssistantStarted);
        let event = |text: &str| {
            if thinking {
                AgentEvent::ThinkingDelta { text: text.into() }
            } else {
                AgentEvent::AssistantDelta {
                    text: text.into(),
                    refusal: false,
                }
            }
        };
        let width = 36;
        let height = 10;
        // Pre-fill a history segment longer than the viewport so any height jitter under bottom alignment shows up in the full-screen diff.
        let seed = (0..8)
            .map(|i| format!("Earlier thought block {i} with enough text to wrap.\n\n"))
            .collect::<String>();
        transcript.progress(event(&seed));
        let mut prev_native = wait_exact_native(&mut transcript, width, height);
        let mut prev_height = transcript.entries[0].layout.as_ref().unwrap().height;

        // Cover in turn: a long unfinished list line wrapping, a single-newline SoftBreak, a double-newline paragraph break and a wrapping quote.
        let chunks = [
            "- Analyzing the ",
            "streaming layout ",
            "worker when a single ",
            "unfinished line keeps ",
            "appending tokens\n",
            "soft continuation",
            "\n\n",
            "> Quoted reasoning ",
            "segment wrapping cleanly",
        ];
        for chunk in chunks {
            let pause = worker.pause();
            transcript.progress(event(chunk));
            let pending_native = render_native(&mut transcript, width, height);
            let pending_alt = render(&mut transcript, width, height);
            let pending_height = transcript.entries[0].layout.as_ref().unwrap().height;
            pause.wait();
            drop(pause);
            let exact_native = wait_exact_native(&mut transcript, width, height);
            let exact_alt = wait_exact(&mut transcript, width);
            let exact_height = transcript.entries[0].layout.as_ref().unwrap().height;

            // The preview frame and the finished background frame must match cell by cell: no refresh of the rows above and no vertical jumping.
            assert_eq!(
                pending_native, exact_native,
                "main-screen preview differs from the background writeback: thinking={thinking}, chunk={chunk:?}"
            );
            assert_eq!(
                pending_alt,
                render(&mut transcript, width, height),
                "alternate-screen preview differs from the background writeback: thinking={thinking}, chunk={chunk:?}"
            );
            let _ = exact_alt;
            assert_eq!(
                pending_height, exact_height,
                "preview and background total heights differ: thinking={thinking}, chunk={chunk:?}"
            );
            assert!(
                exact_height >= prev_height,
                "append-only streaming output must not shrink in height: prev={prev_height}, now={exact_height}, chunk={chunk:?}"
            );

            // If this frame added no line (output continues on the same line), only the active row may change and zero rows above may refresh.
            if exact_height == prev_height {
                let changed_rows: Vec<u16> = prev_native
                    .diff(&exact_native)
                    .into_iter()
                    .map(|(_, y, _)| y)
                    .collect();
                let active_y = height - 1;
                assert!(
                    changed_rows.iter().all(|&y| y == active_y),
                    "no row above may refresh while the line is unfinished and no line was added: changed_rows={changed_rows:?}, chunk={chunk:?}"
                );
            }
            prev_native = exact_native;
            prev_height = exact_height;
        }
    }
}

#[test]
fn streaming_table_transition_to_paragraph_does_not_collapse_into_tail_line() {
    let worker = Worker::start().unwrap();
    let mut transcript = Transcript::new();
    let table = "| A | B |\n| --- | --- |\n| 1 | 2 |\n\n";
    transcript.progress(AgentEvent::ThinkingDelta { text: table.into() });
    wait_exact(&mut transcript, 30);

    let pause = worker.pause();
    transcript.progress(AgentEvent::ThinkingDelta {
        text: "Next paragraph after table".into(),
    });
    let pending = render(&mut transcript, 30, 10);
    pause.wait();
    drop(pause);
    let exact = wait_exact(&mut transcript, 30);
    // render/wait_exact default to height=8, so compare at the same height.
    let _ = exact;
    let exact = render(&mut transcript, 30, 10);
    assert_eq!(
        pending, exact,
        "the preview must match the background layout once a new paragraph starts after a table"
    );
}
