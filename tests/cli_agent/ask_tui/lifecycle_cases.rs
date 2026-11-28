// —— Coverage: draft/cursor/paste preserved, Esc cancel skips the write in the same batch and history stays paired ——

#[test]
fn ask_tui_esc_cancel_keeps_draft_skips_batch_write_and_pairs_history() {
    let protocol = "response";
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = ask_tui_start(&server, home.path(), project.path(), protocol);
        pty.ready();
        pty.input("ask esc task\r");
        // Hold the request without replying first: the worker is waiting on SSE, so the typed draft must survive.
        let (request, _) = pty.request_at(&server, api_path(protocol));
        pty.input("排队草稿");
        pty.draft("排队草稿");
        ask_reply_calls(
            request,
            protocol,
            json!([
                ask_call_json(protocol, "ask-esc", ask_single_arguments()),
                ask_call_json(
                    protocol,
                    "ask-esc-write",
                    json!({"path":"later.txt","content":"x"})
                )
            ]),
        );
        ask_wait_panel(&mut pty, "Which database should the project use?");
        pty.draft("排队草稿");
        // A paste inside the panel never reaches the main draft; assert it never showed up once the cancel settles.
        pty.input("\x1b[200~blocked-paste\x1b[201~");
        pty.input("\x1b");
        cancel_wait_cancelled_ready(&mut pty);
        pty.draft("排队草稿");
        ask_assert_cursor_at_draft_end(&pty);
        {
            let text = pty.parser.screen().contents();
            assert!(
                !text.contains("blocked-paste"),
                "paste inside the panel must not reach the main draft\n{text}"
            );
        }
        assert!(
            std::fs::read(project.path().join("later.txt")).is_err(),
            "cancelled batch must not execute the later write"
        );
        // The cancel itself triggers no HTTP.
        cancel_assert_no_request(&server);

        // The next task submits normally; both call ids stay paired with their results in a stable order.
        ask_clear_draft(&mut pty);
        pty.input("after esc task\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["ask esc task", "after esc task"],
            "protocol {protocol}"
        );
        assert_eq!(
            cancel_tool_outputs(&body, protocol),
            vec![
                (
                    "ask-esc".to_owned(),
                    json!({"ok":false,"cancelled":true,
                           "error":"Tool cancelled; side effects may have occurred."})
                ),
                (
                    "ask-esc-write".to_owned(),
                    json!({"ok":false,"cancelled":true,
                           "error":"Not executed: task cancelled."})
                )
            ]
        );
        assert!(
            std::fs::read(project.path().join("later.txt")).is_err(),
            "write must stay skipped on the next request too"
        );
        ask_final_reply(request, protocol, "after esc done");
        pty.text("after esc done");
        pty.ready();
        pty.input("\x03");
        pty.finish();
        no_more(&server);
}

// —— Coverage: Ctrl-C exits immediately and restores the terminal while waiting on ask ——

// —— Coverage: the TUI schema still offers ask after /new, a cross-protocol /model, and -r resume ——

fn ask_schema_offered_tui(body: &Value, protocol: &str) -> bool {
    body["tools"].as_array().is_some_and(|tools| {
        tools.iter().any(|tool| {
            if protocol == "completion" {
                tool["function"]["name"] == "ask"
            } else {
                tool["name"] == "ask"
            }
        })
    })
}

// —— Coverage: replay restores the question card only, never reopens the interaction, and answers stay out of input history ——

// —— Coverage: a log with ToolStarted but no result restores as an interruption notice without reopening the panel ——

