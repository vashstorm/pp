fn runtime_reasoning_delta(stream: &mut SseReply, protocol: &str, text: &str) {
    if protocol == "completion" {
        stream.event(chat_chunk(
            json!({"role":"assistant","reasoning_content":text}),
            None,
        ));
    } else {
        stream.event(json!({"type":"response.reasoning_summary_text.delta","sequence_number":1,"item_id":"r","output_index":0,"summary_index":0,"delta":text}));
    }
}

#[test]
fn tui_runtime_responsiveness_continuous_thinking_unicode_paste_and_cursor() {
    for protocol in ["completion", "response"] {
        for code in [false, true] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                config_path(dir.path(), "settings.yml"),
                r#"{"tui":{"max_fps":60}}"#,
            )
            .unwrap();
            std::fs::write(
                config_path(dir.path(), "models.yml"),
                response_models(&server, Some(protocol)),
            )
            .unwrap();
            let mut pty = Pty::start_with_existing_files(dir.path());
            pty.ready();
            pty.input("inspect\r");
            let path = if protocol == "completion" {
                "/v1/chat/completions"
            } else {
                "/v1/responses"
            };
            let (request, _) = receive_at(&server, path);
            let mut stream = SseReply::new(request);
            let prefix = if code {
                format!(
                    "```rust\n{}",
                    "let value = items.iter().map(|x| x + 1).collect::<Vec<_>>();\n".repeat(450)
                )
            } else {
                "正在检查输入事件和终端光标是否保持可用。\n".repeat(800)
            };
            runtime_reasoning_delta(&mut stream, protocol, &prefix);
            pty.text(if code { "collect" } else { "终端光标" });
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_writer = stop.clone();
            let producer = thread::spawn(move || {
                for _ in 0..1800 {
                    if stop_writer.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    runtime_reasoning_delta(&mut stream, protocol, "x");
                    // 模拟连续 SSE 数据；屏幕断言验证实际输入完成。
                    thread::sleep(Duration::from_millis(10));
                }
                stream
            });
            for (kind, input, expected, column) in [
                ("ascii", "abc", "abc", 5),
                ("unicode", "中文", "abc中文", 9),
                (
                    "paste",
                    "\x1b[200~粘贴甲乙🙂\x1b[201~",
                    "abc中文粘贴甲乙🙂",
                    19,
                ),
            ] {
                let started = Instant::now();
                pty.input(input);
                pty.draft(expected);
                pty.wait_for(
                    "思考输出期间输入光标保持在中文草稿末尾",
                    |screen| {
                        !screen.hide_cursor()
                            && cancel_cursor_on_input_row(screen)
                            && screen.cursor_position().1 == column
                    },
                );
                println!(
                    "DIAGNOSTIC protocol={protocol} code={code} input={kind} latency_ms={} cursor_col={column}",
                    started.elapsed().as_millis()
                );
            }
            let mut expected = "abc中文粘贴甲乙🙂".to_owned();
            let mut latencies = Vec::new();
            for index in 0..20 {
                let value = if index % 2 == 0 { "中" } else { "x" };
                let started = Instant::now();
                pty.input(value);
                expected.push_str(value);
                pty.draft(&expected);
                latencies.push(started.elapsed());
            }
            latencies.sort();
            if std::env::var_os("PP_TUI_PERF_ASSERT").is_some() {
                assert!(latencies[18] <= Duration::from_millis(50));
            }
            println!(
                "METRIC protocol={protocol} code={code} samples=20 input_p95_ms={}",
                latencies[18].as_millis()
            );
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let mut stream = producer.join().unwrap();
            runtime_reasoning_delta(&mut stream, protocol, " render-marker");
            pty.text("render-marker");
            pty.draft(&expected);
            assert!(!pty.parser.screen().hide_cursor());
            assert_eq!(pty.parser.screen().cursor_position().1, 49);
            let cancelled = Instant::now();
            pty.input("\x1b");
            pty.text("Cancelled");
            if std::env::var_os("PP_TUI_PERF_ASSERT").is_some() {
                assert!(cancelled.elapsed() <= Duration::from_millis(100));
            }
            println!(
                "METRIC protocol={protocol} code={code} esc_ms={}",
                cancelled.elapsed().as_millis()
            );
            pty.draft(&expected);
            pty.input("\x03");
            pty.finish();
            drop(stream);
            no_more(&server);
        }
    }
}
