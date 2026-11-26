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
                "同一半行在后台排版前后的画面应相同"
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
fn streaming_scroll_live_tail_updates_before_background_layout_finishes() {
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
    assert!(transcript.geometry.is_empty());
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
        assert!(Instant::now() < deadline, "后台排版应完成");
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
        // 预先填满超过视口的历史段，确保底部对齐下任何高度抖动都会暴露在整屏 diff 中。
        let seed = (0..8)
            .map(|i| format!("Earlier thought block {i} with enough text to wrap.\n\n"))
            .collect::<String>();
        transcript.progress(event(&seed));
        let mut prev_native = wait_exact_native(&mut transcript, width, height);
        let mut prev_height = transcript.entries[0].layout.as_ref().unwrap().height;

        // 依次覆盖：未结束的列表长行折行、单换行 SoftBreak、双换行分段、引用折行。
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

            // 预览帧与后台完成帧必须逐单元格一致：不刷新上方行、不上下跳动。
            assert_eq!(
                pending_native, exact_native,
                "主屏幕预览与后台回写不一致：thinking={thinking}, chunk={chunk:?}"
            );
            assert_eq!(
                pending_alt,
                render(&mut transcript, width, height),
                "备用屏幕预览与后台回写不一致：thinking={thinking}, chunk={chunk:?}"
            );
            let _ = exact_alt;
            assert_eq!(
                pending_height, exact_height,
                "预览与后台总高度不一致：thinking={thinking}, chunk={chunk:?}"
            );
            assert!(
                exact_height >= prev_height,
                "纯追加流式输出不应高度回缩：prev={prev_height}, now={exact_height}, chunk={chunk:?}"
            );

            // 若本帧高度未增行（同一行内继续输出），只有当前活跃行允许变更，上方行 0 刷新。
            if exact_height == prev_height {
                let changed_rows: Vec<u16> = prev_native
                    .diff(&exact_native)
                    .into_iter()
                    .map(|(_, y, _)| y)
                    .collect();
                let active_y = height - 1;
                assert!(
                    changed_rows.iter().all(|&y| y == active_y),
                    "行未结束且未增行时不应刷新上方行：changed_rows={changed_rows:?}, chunk={chunk:?}"
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
    // render/wait_exact 默认 height=8，用同一高度对比。
    let _ = exact;
    let exact = render(&mut transcript, 30, 10);
    assert_eq!(pending, exact, "表格后起新段落时预览应与后台一致");
}
