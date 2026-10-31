// 真实 SGR 滚轮与持续 SSE 输出共同驱动；截图只比较固定历史区的文本和样式。
fn scroll_stream_region(screen: &vt100::Screen) -> Vec<u8> {
    screen.rows_formatted(0, 100).take(8).flatten().collect()
}

fn scroll_stream_case(thinking: bool) {
    for protocol in ["completion", "response"] {
        for code in [false, true] {
            let provider = server();
            let (_home, mut pty) =
                runtime_input_start(&provider, protocol, "tui:\n  max_fps: 60\n");
            pty.input("scroll stream task\r");
            let (request, _) = pty.request_at(&provider, api_path(protocol));
            let mut stream = SseReply::new(request);
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
            let send = |stream: &mut SseReply, text: &str| {
                if thinking {
                    runtime_reasoning_delta(stream, protocol, text);
                } else {
                    streaming_delta(stream, protocol, text);
                }
            };
            send(&mut stream, &prefix);
            pty.wait_for("完整 Markdown 已排版", |screen| {
                screen.contents().contains("row_0599")
                    && if code {
                        label_color(screen, "let row_0599") == vt100::Color::Rgb(156, 220, 254)
                    } else {
                        !screen.contents().contains("**")
                    }
            });
            let raw_start = pty.raw.len();
            pty.input("\x1b[<64;20;10M\x1b[<64;20;10M\x1b[<64;20;10M");
            pty.wait_for("滚轮进入历史浏览", |screen| {
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
                    // 模拟供应商分片速率，正确性由终端内容与最终标记确认。
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
                    "持续输出时历史区应稳定：{protocol} thinking={thinking} code={code}\n{}",
                    pty.diagnostics()
                );
                assert!(pty.parser.screen().alternate_screen());
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let mut stream = producer.join().unwrap();
            send(&mut stream, "\nscroll-stream-final-marker\n");
            pty.input(&"\x1b[6~".repeat(100));
            pty.wait_for("回到底部展示新增内容", |screen| {
                !screen.alternate_screen()
                    && screen.contents().contains("scroll-stream-final-marker")
            });
            pty.draft(&draft);
            let bytes = String::from_utf8_lossy(&pty.raw[raw_start..]);
            assert_eq!(
                bytes.matches("\x1b[?1049h").count(),
                1,
                "浏览中不得反复进入备用屏幕"
            );
            assert_eq!(
                bytes.matches("\x1b[?1049l").count(),
                1,
                "回到底部只退出一次备用屏幕"
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
}

#[test]
fn tui_scrolled_stream_assistant_keeps_history_and_input_both_protocols() {
    scroll_stream_case(false);
}

#[test]
fn tui_scrolled_stream_thinking_keeps_history_and_input_both_protocols() {
    scroll_stream_case(true);
}
