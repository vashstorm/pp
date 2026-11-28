use super::*;

#[test]
fn thinking_and_bash_cards_stream_with_default_style_and_expand_without_editing_draft() {
    let protocol = "response";
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
    let mut stream = request.start_sse().unwrap();
    if protocol == "completion" {
        stream
            .event(&chat_chunk(
                json!({"role":"assistant","reasoning_content":"Check the diff 中"}),
                None,
            ))
            .unwrap();
    } else {
        assert_eq!(body["reasoning"]["summary"], "concise");
        stream.event(&json!({"type":"response.reasoning_summary_text.delta","sequence_number":1,"item_id":"r","output_index":0,"summary_index":0,"delta":"Check the diff 中"})).unwrap();
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
        vt100::Color::Rgb(40, 42, 53)
    );
    if protocol == "completion" {
        streaming_delta(&mut stream, protocol, "inspected");
        streaming_complete(&mut stream, protocol, "inspected");
    } else {
        stream.event(&json!({"type":"response.completed","sequence_number":2,"response":response_body(json!([
                    {"type":"reasoning","id":"r","status":"completed","summary":[{"type":"summary_text","text":"Check the diff 中"}]},
                    response_message("m",json!([response_text("inspected")]))
                ]))})).unwrap();
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
    pty.text("Wall:");
    pty.text("9 earlier lines");
    pty.text("Output");
    assert!(!pty.parser.screen().contents().contains("output-00"));
    pty.input("draft-中");
    pty.draft("draft-中");
    // Option/Alt+O no longer expands tool output; every platform uses Ctrl+O.
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
        let mut pty = Pty::start_with_args(dir.path(), dir.path(), &["--yolo"]);
        pty.ready();
        pty.input("live task\r");
        let path = if protocol == "completion" {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        };
        let (request, body) = receive_at(&server, path);
        assert_eq!(body["stream"], true);
        let mut stream = request.start_sse().unwrap();
        streaming_delta(&mut stream, protocol, "first-中");
        pty.text("first-中");
        pty.wait_for("the streaming reply status carries a clock", |screen| {
            live_activity_labels(&screen.contents()).any(|label| label == "Responding")
        });
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
        // The activity line always takes one row and has one blank separator row above it (since 252f311). When streaming ends
        // both rows are reclaimed and the transcript grows, so "the scrolled-up viewport does not jump" can only compare rows
        // that belong to the transcript both before and after completion; otherwise rows revealed by the taller viewport would
        // be misread as a pull back to the bottom.
        let before: Vec<String> = pty
            .parser
            .screen()
            .contents()
            .lines()
            .map(str::to_owned)
            .collect();
        let activity = before
            .iter()
            .position(|row| row.contains("Responding"))
            .expect("the streaming status line must show in the activity area");
        assert!(
            activity > 1,
            "the transcript must already have visible rows after paging up: {before:?}"
        );
        let old_view = before[..activity - 1].to_vec();
        streaming_delta(&mut stream, protocol, "-last");
        full.push_str("-last");
        streaming_complete(&mut stream, protocol, &full);
        stream.finish();
        pty.ready();
        let after: Vec<String> = pty
            .parser
            .screen()
            .contents()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(
            after[..old_view.len()],
            old_view[..],
            "the viewport must stay in place while paged up"
        );
        assert!(
            !pty.parser.screen().contents().contains("line-59-last"),
            "completion must not jump to the bottom"
        );
        pty.draft("draft-中");
        pty.input("\x1b[6~");
        pty.text("line-59-last");
        // The session dialog appears after the first task completes; page to the bottom and then verify the next task follows the output.
        pty.input("\x1b[6~");
        pty.completed();
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
            text.contains("out-first") && text.contains("err-first")
        });
        pty.drain();
        assert!(input_is_ready(pty.parser.screen()));
        assert!(!pty.parser.screen().contents().contains("out-firstout-last"));
        pty.wait_for("exactly one live status while Bash runs", |screen| {
            live_activity_labels(&screen.contents()).eq(["Running Bash"])
        });
        no_more(&server);
        std::fs::write(dir.path().join("release"), "").unwrap();
        let (request, body) = receive_at(&server, path);
        let tool = if protocol == "completion" {
            result(body["messages"].as_array().unwrap().last().unwrap(), "b")
        } else {
            response_result(body["input"].as_array().unwrap().last().unwrap(), "b")
        };
        let combined = tool["output"].as_str().unwrap();
        assert!(
            combined.contains("out-first")
                && combined.contains("out-last")
                && combined.contains("err-first")
        );
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
