// Driven by a real SGR wheel and continuous SSE output together; the screenshot compares only the text and
// styles of the fixed history region. The row bodies end in a wide CJK character plus an emoji, and the draft
// appended below is CJK too, so the region stability check covers wide-character layout.
fn scroll_stream_region(screen: &vt100::Screen) -> Vec<u8> {
    screen.rows_formatted(0, 100).take(8).flatten().collect()
}

fn scroll_stream_case(thinking: bool) {
    let protocol = "response";
        for code in [false, true] {
            let provider = server();
            let (_home, mut pty) =
                runtime_input_start(&provider, protocol, "tui:\n  max_fps: 60\n");
            pty.input("scroll stream task\r");
            let (request, _) = pty.request_at(&provider, api_path(protocol));
            let mut stream = request.start_sse().unwrap();
            let prefix = if code {
                format!(
                    "```rust\n{}",
                    (0..600)
                        .map(|i| format!("let row_{i:04} = \"中文🙂\";\n"))
                        .collect::<String>()
                )
            } else {
                (0..600)
                    .map(|i| format!("- **row_{i:04}** `中文🙂`\n"))
                    .collect::<String>()
            };
            let send = |stream: &mut SseWriter, text: &str| {
                if thinking {
                    runtime_reasoning_delta(stream, protocol, text);
                } else {
                    streaming_delta(stream, protocol, text);
                }
            };
            send(&mut stream, &prefix);
            pty.wait_for("complete Markdown is laid out", |screen| {
                let text = screen.contents();
                text.contains("row_0599")
                    && if code {
                        label_color(screen, "let row_0599") == vt100::Color::Rgb(156, 220, 254)
                    } else {
                        text.contains("• row_0598 中文🙂")
                            && !text
                                .lines()
                                .filter(|line| !line.contains("row_0599"))
                                .any(|line| line.contains("**"))
                    }
            });
            let raw_start = pty.raw.len();
            pty.input("\x1b[<64;20;10M\x1b[<64;20;10M\x1b[<64;20;10M");
            pty.wait_for("the wheel enters history browsing", |screen| {
                screen.alternate_screen()
            });
            pty.input("anchor");
            pty.draft("anchor");
            let before = scroll_stream_region(pty.parser.screen());
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let producer_stop = stop.clone();
            let producer = thread::spawn(move || {
                for _ in 0..400 {
                    if producer_stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    if thinking {
                        runtime_reasoning_delta(&mut stream, protocol, "tail\n");
                    } else {
                        streaming_delta(&mut stream, protocol, "tail\n");
                    }
                    // Simulate the provider fragment rate; correctness is confirmed by the terminal content and the
                    // final marker.
                    thread::sleep(Duration::from_millis(10));
                }
                stream
            });
            let mut draft = "anchor".to_owned();
            for _ in 0..16 {
                pty.input("中");
                draft.push('中');
                pty.draft(&draft);
                assert_eq!(
                    scroll_stream_region(pty.parser.screen()),
                    before,
                    "the history region must stay stable during continuous output: {protocol} thinking={thinking} code={code}\n{}",
                    pty.diagnostics()
                );
                assert!(pty.parser.screen().alternate_screen());
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let mut stream = producer.join().unwrap();
            send(&mut stream, "\nscroll-stream-final-marker\n");
            pty.input(&"\x1b[6~".repeat(100));
            pty.wait_for("returning to the bottom shows the new content", |screen| {
                !screen.alternate_screen()
                    && screen.contents().contains("scroll-stream-final-marker")
            });
            pty.draft(&draft);
            let bytes = String::from_utf8_lossy(&pty.raw[raw_start..]);
            assert_eq!(
                bytes.matches("\x1b[?1049h").count(),
                1,
                "browsing must not re-enter the alternate screen repeatedly"
            );
            assert_eq!(
                bytes.matches("\x1b[?1049l").count(),
                1,
                "returning to the bottom leaves the alternate screen exactly once"
            );
            pty.input("\x1b");
            pty.text("Cancelled");
            pty.draft(&draft);
            pty.input("\x03");
            pty.finish();
            drop(stream);
            no_more(&provider);
        }
}

#[test]
fn tui_scrolled_stream_assistant_keeps_history_and_input_both_protocols() {
    scroll_stream_case(false);
}

