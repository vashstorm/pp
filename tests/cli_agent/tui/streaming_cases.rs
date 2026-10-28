use super::*;

#[test]
fn thinking_and_bash_cards_stream_with_default_style_and_expand_without_editing_draft() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(dir.path(), "settings.yml"),
            r#"{"think_level":"high"}"#,
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
        let (request, body) = receive_at(&server, path);
        let mut stream = SseReply::new(request);
        if protocol == "completion" {
            stream.event(chat_chunk(
                json!({"role":"assistant","reasoning_content":"Check the diff 中"}),
                None,
            ));
        } else {
            assert_eq!(body["reasoning"]["summary"], "auto");
            stream.event(json!({"type":"response.reasoning_summary_text.delta","sequence_number":1,"item_id":"r","output_index":0,"summary_index":0,"delta":"Check the diff 中"}));
        }
        pty.text("Check the diff 中");
        let row = (0..30)
            .find(|row| {
                (0..100)
                    .filter_map(|col| pty.parser.screen().cell(*row, col))
                    .map(|cell| cell.contents())
                    .collect::<String>()
                    .contains("Check the diff")
            })
            .unwrap();
        assert_eq!(pty.parser.screen().cell(row, 0).unwrap().contents(), " ");
        assert_eq!(
            pty.parser.screen().cell(row, 2).unwrap().fgcolor(),
            vt100::Color::Rgb(110, 117, 129)
        );
        assert_eq!(
            pty.parser.screen().cell(row, 2).unwrap().bgcolor(),
            vt100::Color::Rgb(35, 39, 50)
        );
        if protocol == "completion" {
            streaming_delta(&mut stream, protocol, "inspected");
            streaming_complete(&mut stream, protocol, "inspected");
        } else {
            stream.event(json!({"type":"response.completed","sequence_number":2,"response":response_body(json!([
                    {"type":"reasoning","id":"r","status":"completed","summary":[{"type":"summary_text","text":"Check the diff 中"}]},
                    response_message("m",json!([response_text("inspected")]))
                ]))}));
        }
        stream.finish();
        pty.text("inspected");
        pty.completed();
        pty.input("run bash\r");
        let (request, _) = receive_at(&server, path);
        let command = "for i in {0..18}; do printf 'output-%02d\\n' \"$i\"; done";
        if protocol == "completion" {
            calls_reply(
                request,
                json!([call("b", "bash", json!({"command":command}))]),
            );
        } else {
            response_reply(
                request,
                json!([response_call("b", "bash", json!({"command":command}))]),
            );
        }
        let (request, _) = receive_at(&server, path);
        pty.text("Exit code: 0");
        pty.text("9 earlier lines");
        pty.text("Output");
        assert!(!pty.parser.screen().contents().contains("output-00"));
        assert_eq!(
            label_color(pty.parser.screen(), "$ for"),
            vt100::Color::Rgb(190, 188, 139)
        );
        pty.input("draft-中");
        pty.draft("draft-中");
        // Option/Alt+O 不再展开工具输出，所有平台统一使用 Ctrl+O。
        pty.input("\x1bo");
        pty.input("!");
        pty.draft("draft-中!");
        assert!(!pty.parser.screen().contents().contains("output-00"));
        pty.input("\x7f");
        pty.draft("draft-中");
        pty.function_key('o');
        pty.text("output-00");
        pty.draft("draft-中");
        pty.function_key('o');
        pty.text("9 earlier lines");
        pty.draft("draft-中");
        pty.resize(4, 20);
        pty.resize(30, 100);
        pty.text("output-18");
        pty.draft("draft-中");
        if protocol == "completion" {
            final_reply(request, "done");
        } else {
            response_final(request, "done");
        }
        pty.text("done");
        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn streaming_tui_both_protocols_live_colors_scroll_resize_and_bash() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(config_path(dir.path(), "settings.yml"), fixture_settings()).unwrap();
        std::fs::write(
            config_path(dir.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_existing_files(dir.path());
        pty.ready();
        pty.input("live task\r");
        let path = if protocol == "completion" {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        };
        let (request, body) = receive_at(&server, path);
        assert_eq!(body["stream"], true);
        let mut stream = SseReply::new(request);
        streaming_delta(&mut stream, protocol, "first-中");
        pty.text("first-中");
        pty.text("✻ Responding ·");
        pty.drain();
        assert!(input_is_ready(pty.parser.screen()));
        assert!(!pty.parser.screen().contents().contains("-last"));
        assert_eq!(
            background_color(pty.parser.screen(), "live task"),
            USER_BACKGROUND
        );
        assert_eq!(
            background_color(pty.parser.screen(), "first-中"),
            ASSISTANT_BACKGROUND
        );
        let mut full = "first-中".to_owned();
        let lines = format!(
            "\n\n```text\n{}",
            (0..60)
                .map(|i| format!("line-{i:02}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        full.push_str(&lines);
        streaming_delta(&mut stream, protocol, &lines);
        pty.text("line-59");
        pty.input("draft-中");
        pty.draft("draft-中");
        pty.input("\x1b[5~");
        pty.wait_for("paged older output", |s| {
            s.contents().contains("line-30") && !s.contents().contains("line-59")
        });
        let old_view: Vec<String> = pty
            .parser
            .screen()
            .contents()
            .lines()
            .take(25)
            .map(str::to_owned)
            .collect();
        streaming_delta(&mut stream, protocol, "-last");
        full.push_str("-last");
        streaming_complete(&mut stream, protocol, &full);
        stream.finish();
        pty.ready();
        assert_eq!(
            pty.parser
                .screen()
                .contents()
                .lines()
                .take(25)
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            old_view
        );
        pty.draft("draft-中");
        pty.input("\x1b[6~");
        pty.text("line-59-last");
        // 首轮任务完成后显示会话弹框，翻到底部后再验证下一任务跟随输出。
        pty.input("\x1b[6~");
        pty.text("Session:");
        pty.resize(4, 20);
        pty.resize(30, 100);
        pty.draft("draft-中");
        pty.text("line-59-last");
        pty.input("\r");
        let (request, _) = receive_at(&server, path);
        let command = "printf 'out-first'; printf 'err-first' >&2; for ((i=0;i<1000;i++)); do [[ -e release ]] && break; sleep 0.01; done; printf 'out-last'";
        if protocol == "completion" {
            calls_reply(
                request,
                json!([call("b", "bash", json!({"command":command}))]),
            );
        } else {
            response_reply(
                request,
                json!([response_call("b", "bash", json!({"command":command}))]),
            );
        }
        pty.wait_for("both live pipe bodies", |s| {
            let text = s.contents();
            let body = |expected: &str| {
                // 右侧通知可覆盖同一行，按左侧可见的管道正文断言。
                text.lines().any(|line| {
                    line.trim_start_matches([' ', '│']).split([' ', '│']).next() == Some(expected)
                })
            };
            body("out-first") && body("err-first")
        });
        pty.drain();
        assert!(input_is_ready(pty.parser.screen()));
        assert!(!pty.parser.screen().contents().contains("out-firstout-last"));
        let bash_color = label_color(pty.parser.screen(), "$ printf");
        assert_eq!(bash_color, vt100::Color::Rgb(190, 188, 139));
        pty.text("✻ Running bash ·");
        assert_eq!(pty.parser.screen().contents().matches('✻').count(), 1);
        no_more(&server);
        std::fs::write(dir.path().join("release"), "").unwrap();
        let (request, body) = receive_at(&server, path);
        let tool = if protocol == "completion" {
            result(body["messages"].as_array().unwrap().last().unwrap(), "b")
        } else {
            response_result(body["input"].as_array().unwrap().last().unwrap(), "b")
        };
        assert_eq!(tool["stdout"], "out-firstout-last");
        assert_eq!(tool["stderr"], "err-first");
        assert_eq!(tool["exit_code"], 0);
        if protocol == "completion" {
            final_reply(request, "bash-complete");
        } else {
            response_final(request, "bash-complete");
        }
        pty.text("bash-complete");
        pty.ready();
        pty.input("interrupt running\r");
        let (_request, _) = receive_at(&server, path);
        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn streaming_tui_partial_failures_preserve_text_and_recover_without_leaks() {
    for protocol in ["completion", "response"] {
        for ending in ["eof", "json", "failed"] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(config_path(dir.path(), "settings.yml"), fixture_settings()).unwrap();
            std::fs::write(
                config_path(dir.path(), "models.yml"),
                response_models(&server, Some(protocol)),
            )
            .unwrap();
            let mut pty = Pty::start_with_existing_files(dir.path());
            pty.ready();
            pty.input("fail partial\r");
            let path = if protocol == "completion" {
                "/v1/chat/completions"
            } else {
                "/v1/responses"
            };
            let (request, _) = receive_at(&server, path);
            let mut stream = SseReply::new(request);
            streaming_delta(&mut stream, protocol, "partial-visible");
            pty.text("partial-visible");
            match ending {
                "json" => stream.frame("{private-provider-body fixture-key"),
                "failed" if protocol == "completion" => stream.event(json!({
                    "error":{"message":"private-provider-body fixture-key","type":"server_error"}
                })),
                "failed" => stream.event(json!({
                    "type":"error","sequence_number":1,"code":"server_error",
                    "message":"private-provider-body fixture-key","param":null
                })),
                _ => {}
            }
            stream.finish();
            pty.wait_for("部分输出保留且错误显示为红色弹窗", |s| {
                let text = s.contents();
                text.contains("partial-visible")
                    && notification_bounds(s, "Error", vt100::Color::Rgb(224, 108, 117)).is_some()
                    && input_is_ready(s)
            });
            assert!(
                !pty.parser
                    .screen()
                    .contents()
                    .contains("private-provider-body")
            );
            assert!(!String::from_utf8_lossy(&pty.raw).contains("fixture-key"));
            assert!(!String::from_utf8_lossy(&pty.raw).contains("private-provider-body"));
            pty.input("recover\r");
            let (request, body) = receive_at(&server, path);
            if protocol == "completion" {
                assert_eq!(body["messages"].as_array().unwrap().len(), 3);
                assert_eq!(body["messages"][1]["content"], "fail partial");
                assert_eq!(body["messages"][2]["content"], "recover");
                final_reply(request, "recovered");
            } else {
                assert_eq!(body["input"].as_array().unwrap().len(), 2);
                response_user(&body["input"][0], "fail partial");
                response_user(&body["input"][1], "recover");
                response_final(request, "recovered");
            }
            pty.text("recovered");
            pty.ready();
            pty.input("\x03");
            pty.finish();
            no_more(&server);
        }
    }
}
