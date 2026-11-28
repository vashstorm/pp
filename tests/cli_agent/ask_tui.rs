// End-to-end ask TUI regression (each ask row of the plan's verification): multi-question Review editing
// and exact answers, free-form answers with empty options and explicit submission of empty multi-select,
// malformed arguments opening no panel, Esc cancelling while keeping the draft and skipping the write in
// the same batch, Ctrl-C exit while waiting, schema surviving /new, cross-protocol /model and -r restore,
// and replay restoring only the Q&A cards. Everything is observed through the real PTY, loopback HTTP
// and screen/HTTP barriers, never by guessing stages with sleep. Helpers are all named with the `ask_`
// prefix and reuse the existing cancel_/resume_ harness in the same scope (cancel_tool_outputs,
// cancel_wait_cancelled_ready, saved_session_id, api_path, notification_bounds).

// --- Shared fixtures ---

// Two-question batch: storage single-select (recommended=0 is SQLite), checks multi-select.
fn ask_batch_arguments() -> Value {
    json!({"questions":[
        {"id":"storage","header":"Storage","question":"Which database should the project use?",
         "options":[{"label":"SQLite"},{"label":"Postgres"}],"recommended":0},
        {"id":"checks","header":"Checks","question":"Which checks should run?",
         "multi":true,"options":[{"label":"unit"},{"label":"lint"}]}
    ]})
}

fn ask_single_arguments() -> Value {
    json!({"questions":[
        {"id":"storage","question":"Which database should the project use?",
         "options":[{"label":"SQLite"},{"label":"Postgres"}],"recommended":0}
    ]})
}

// The summary does not affect the model body; restore, preview configuration and native scrollback all
// hide the read body.
#[test]
fn tool_folding_omp_read_default_summary_resume_and_native_scrollback() {
    let protocol = "response";
        let provider = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        crate::write_settings(home.path(), r#"{"notification_timeout_seconds":1}"#).unwrap();
        std::fs::write(config_path(home.path(), "models.yml"), response_models(&provider, Some(protocol))).unwrap();
        for name in ["one", "two"] {
            std::fs::write(project.path().join(name), (0..20).map(|i|format!("body-{name}-{i:02}\n")).collect::<String>()).unwrap();
        }
        let mut pty = Pty::start_with_args(project.path(), home.path(), &["--yolo"]);
        pty.ready();
        pty.resize(80, 100);
        pty.input("read summary\r");
        let (request, _) = pty.request_at(&provider, api_path(protocol));
        ask_reply_calls(request, protocol, json!([
            diff_blocks_call(protocol,"one","read",json!({"path":"one"})),
            diff_blocks_call(protocol,"two","read",json!({"path":"two:2"}))
        ]));
        let (request, body) = pty.request_at(&provider, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        assert!(outputs[0].1["content"].as_str().unwrap().contains("body-one-19"));
        assert!(outputs[1].1["content"].as_str().unwrap().starts_with("body-two-01"));
        pty.wait_for("default local read summary", |screen| {
            let text = screen.contents();
            text.contains("Read (2)") && text.contains("one:1-20") && text.contains("two:2-20") && !text.contains("body-one-")
        });
        {
            let screen = pty.parser.screen();
            for (label, color) in [
                ("\u{f00c}", vt100::Color::Rgb(137, 210, 129)),
                ("Read", vt100::Color::Rgb(145, 183, 218)),
                ("(2)", vt100::Color::Rgb(96, 102, 114)),
                ("├─", vt100::Color::Rgb(96, 102, 114)),
                ("one", vt100::Color::Rgb(254, 188, 56)),
                (":1-20", vt100::Color::Rgb(190, 198, 212)),
                ("two", vt100::Color::Rgb(254, 188, 56)),
                (":2-20", vt100::Color::Rgb(190, 198, 212)),
            ] {
                let (row, col) = ask_panel_find(screen, label);
                let cell = screen.cell(row, col).unwrap();
                assert_eq!(cell.fgcolor(), color, "{label}");
                assert_eq!(cell.bgcolor(), vt100::Color::Default, "{label}");
                assert!(!cell.bold(), "{label}");
            }
            assert!(!screen.contents().contains(" · ok"));
        }
        pty.input("barrier");
        pty.draft("barrier");
        ask_final_reply(request, protocol, "summary done");
        pty.text("summary done");
        pty.ready();
        let (scrollback, _) = diff_blocks_exit_scrollback(&mut pty, "summary done");
        assert!(scrollback.contains("Read (2)"));
        assert!(!scrollback.contains("body-one-19") && !scrollback.contains("body-two-19"));
        pty.finish();
        let id = saved_session_id(home.path());
        crate::write_settings(home.path(), r#"{"notification_timeout_seconds":1,"read":{"toolResultPreview":true}}"#).unwrap();
        let mut resumed = Pty::start_with_args(project.path(), home.path(), &["-r", &id]);
        resumed.resize(80, 100);
        resumed.ready();
        resumed.wait_for("restore still shows only the read summary", |screen| {
            let text = screen.contents();
            text.contains("Read (2)") && !text.contains("body-one-") && !text.contains("Ctrl + O to expand")
        });
        resumed.input("\x0fa");
        resumed.draft("a");
        assert!(!resumed.parser.screen().contents().contains("body-one-"));
        resumed.input("\x03");
        resumed.finish();
        no_more(&provider);
}

fn ask_call_json(protocol: &str, id: &str, arguments: Value) -> Value {
    if protocol == "completion" {
        call(id, "ask", arguments)
    } else {
        response_call(id, "ask", arguments)
    }
}

// arguments is a raw string (used for malformed arguments).
fn ask_call_raw(protocol: &str, id: &str, arguments: &str) -> Value {
    if protocol == "completion" {
        json!({"id":id,"type":"function","function":{"name":"ask","arguments":arguments}})
    } else {
        json!({
            "type":"function_call","id":format!("fc-{id}"),"call_id":id,
            "name":"ask","arguments":arguments,"status":"completed"
        })
    }
}

fn ask_reply_calls(request: CapturedRequest, protocol: &str, calls: Value) {
    if protocol == "completion" {
        calls_reply(request, calls);
    } else {
        response_reply(request, calls);
    }
}

// Covers the full argument snapshot and fragmented SSE; the Responses variant that omits arguments on
// its opening item must work as well.
fn ask_provider_reply(request: CapturedRequest, protocol: &str, id: &str, arguments: &str, fragmented: bool) {
    if !fragmented {
        ask_reply_calls(request, protocol, json!([ask_call_raw(protocol, id, arguments)]));
        return;
    }
    let split = arguments.char_indices().nth(arguments.chars().count() / 2).unwrap().0;
    let parts = [&arguments[..split], &arguments[split..]];
    let mut stream = request.start_sse().unwrap();
    if protocol == "completion" {
        stream.event(&chat_chunk(json!({"role":"assistant","tool_calls":[{
            "index":0,"id":id,"type":"function","function":{"name":"ask","arguments":parts[0]}
        }]}), None)).unwrap();
        stream.event(&chat_chunk(json!({"tool_calls":[{
            "index":0,"function":{"arguments":parts[1]}
        }]}), None)).unwrap();
        stream.event(&chat_chunk(json!({}), Some("tool_calls"))).unwrap();
        stream.frame("[DONE]").unwrap();
    } else {
        let item = ask_call_raw(protocol, id, arguments);
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        added.as_object_mut().unwrap().remove("arguments");
        stream.event(&json!({"type":"response.output_item.added","sequence_number":0,
            "output_index":0,"item":added})).unwrap();
        for (index, part) in parts.into_iter().enumerate() {
            stream.event(&json!({"type":"response.function_call_arguments.delta","sequence_number":index+1,
                "item_id":item["id"],"output_index":0,"delta":part})).unwrap();
        }
        stream.event(&json!({"type":"response.function_call_arguments.done","sequence_number":3,
            "item_id":item["id"],"output_index":0,"name":"ask","arguments":arguments})).unwrap();
        stream.event(&json!({"type":"response.output_item.done","sequence_number":4,
            "output_index":0,"item":item})).unwrap();
        stream.event(&json!({"type":"response.completed","sequence_number":5,
            "response":response_body(json!([item]))})).unwrap();
    }
    stream.finish();
}

fn ask_final_reply(request: CapturedRequest, protocol: &str, text: &str) {
    if protocol == "completion" {
        final_reply(request, text);
    } else {
        response_final(request, text);
    }
}

fn ask_tui_start(server: &TestServer, home: &Path, project: &Path, protocol: &str) -> Pty {
    // The popup fades out within a second so it does not race the 5-second barrier.
    crate::write_settings(home,
        r#"{"default_model":"fixture/primary","notification_timeout_seconds":1,"read":{"toolResultPreview":true}}"#,
    )
    .unwrap();
    std::fs::write(
        config_path(home, "models.yml"),
        response_models(server, Some(protocol)),
    )
    .unwrap();
    Pty::start_with_args(project, home, &[])
}

// The message area has no custom entry point; only a rounded Ask title and Other appearing together mean
// the panel is open.
fn ask_wait_panel(pty: &mut Pty, _question: &str) {
    pty.wait_for("ask panel opened", |screen| {
        let text = screen.contents();
        text.contains("Other (type your own)") && text.contains("╭ \u{f02d7} Ask")
    });
}

fn ask_wait_review(pty: &mut Pty) {
    pty.wait_for("ask review page", |screen| {
        let text = screen.contents();
        text.contains("╭ \u{f02d7} Ask · Submit")
            && text.contains("Enter submit · Tab questions · Esc cancel")
    });
}

fn ask_wait_editor(pty: &mut Pty) {
    pty.wait_for("ask custom editor opened", |screen| {
        screen.contents().contains("Enter accept · Ctrl + J newline · Esc discard")
    });
}

// The panel sits below the transcript, so locate it from the bottom to avoid misreading duplicated
// questions and options inside the transcript.
fn ask_panel_find(screen: &vt100::Screen, needle: &str) -> (u16, u16) {
    let (rows, cols) = screen.size();
    for row in (0..rows).rev() {
        let line: String = (0..cols)
            .filter_map(|col| screen.cell(row, col))
            .map(|cell| {
                let text = cell.contents();
                if text.is_empty() { " " } else { text }
            })
            .collect();
        if let Some(index) = line.find(needle) {
            return (row, line[..index].chars().count() as u16);
        }
    }
    panic!("panel did not show {needle}: {}", screen.contents());
}

fn ask_page_active(screen: &vt100::Screen, index: usize, total: usize) -> bool {
    screen.contents().contains(&format!("╭ \u{f02d7} Ask · {index}/{total} "))
}

fn ask_clear_draft(pty: &mut Pty) {
    pty.input("\x01\x0b");
    pty.draft("");
}

// The cursor must rest at the end of the draft on the input row: the ❯ prefix takes 2 columns and a
// four-wide-character queued draft takes 8.
fn ask_assert_cursor_at_draft_end(pty: &Pty) {
    let screen = pty.parser.screen();
    let (row, column) = screen.cursor_position();
    let input_row = screen
        .contents()
        .lines()
        .position(|line| line.starts_with('❯'))
        .expect("input row with ❯ prompt");
    assert_eq!(usize::from(row), input_row, "{}", pty.diagnostics());
    assert_eq!(column, 10, "{}", pty.diagnostics());
}

// --- Verification items: multiple questions, Review edits, exact answers (both protocols) ---

#[test]
fn ask_tui_two_questions_review_modify_and_exact_answers() {
    let protocol = "response";
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = ask_tui_start(&server, home.path(), project.path(), protocol);
        pty.ready();
        pty.input("ask roundtrip task\r");
        let (request, _) = pty.request_at(&server, api_path(protocol));
        ask_reply_calls(
            request,
            protocol,
            json!([ask_call_json(protocol, "ask-review", ask_batch_arguments())]),
        );
        ask_wait_panel(&mut pty, "Which database should the project use?");
        // The recommendation is only the focus: SQLite carries a (Recommended) badge but is not preselected.
        {
            let text = pty.parser.screen().contents();
            assert!(
                text.contains("󰐽 SQLite (Recommended)") && text.contains("󰐽 Postgres"),
                "recommendation must be a display badge only\n{text}"
            );
            assert!(
                ask_page_active(pty.parser.screen(), 1, 2),
                "the Ask title must show its icon and the multi-question progress\n{text}"
            );
        }

        // Q1: after the focus moves to Postgres, Enter selects it and advances.
        pty.input("\x1b[B\r");
        pty.wait_for("second question page", |screen| {
            ask_page_active(screen, 2, 2)
        });
        // Q2: Space only toggles and Enter only advances.
        pty.input(" ");
        pty.wait_for("unit checked", |screen| screen.contents().contains("󰄲 unit"));
        pty.input("\x1b[B ");
        pty.wait_for("lint checked", |screen| screen.contents().contains("󰄲 lint"));

        // A short-screen resize does not clear the selection set: compact mode (no borders) still shows
        // the selected summary.
        pty.resize(8, 40);
        pty.wait_for("compact panel keeps summary", |screen| {
            screen.contents().contains("unit, lint")
        });
        pty.resize(30, 100);
        pty.wait_for("resize keeps selections", |screen| {
            screen.contents().contains("󰄲 lint")
        });
        pty.input("\r");
        ask_wait_review(&mut pty);
        {
            let text = pty.parser.screen().contents();
            assert!(
                text.contains("Postgres") && text.contains("unit, lint"),
                "review must summarize current answers\n{text}"
            );
        }

        // Review returning to a question: Q1 keeps Postgres selected and SQLite still unselected.
        pty.input("\t");
        pty.wait_for("back to first question", |screen| {
            ask_page_active(screen, 1, 2)
        });
        {
            let text = pty.parser.screen().contents();
            assert!(
                text.contains("󰐾 Postgres") && text.contains("󰐽 SQLite (Recommended)"),
                "selection and focus must survive page switches\n{text}"
            );
            let screen = pty.parser.screen();
            let (row, col) = ask_panel_find(screen, "󰐾 Postgres");
            assert_eq!(screen.cell(row, col).unwrap().fgcolor(), vt100::Color::Rgb(137, 210, 129));
        }
        pty.input("\t");
        pty.wait_for("back to second question", |screen| {
            ask_page_active(screen, 2, 2)
        });
        // The focus is still on lint: Space unchecks and then rechecks, so answers can be changed
        // repeatedly before submitting.
        pty.input(" ");
        pty.wait_for("lint unchecked", |screen| screen.contents().contains("󰄱 lint"));
        pty.input(" ");
        pty.wait_for("lint rechecked", |screen| screen.contents().contains("󰄲 lint"));
        pty.input("\r");
        ask_wait_review(&mut pty);
        pty.wait_for("review shows both checks again", |screen| {
            screen.contents().contains("unit, lint")
        });

        // Add a custom answer to Q2: Enter opens the editor, Ctrl-J inserts a newline and Enter accepts
        // non-blank content.
        pty.input("\t\t");
        pty.wait_for("back to second question again", |screen| {
            ask_page_active(screen, 2, 2)
        });
        pty.input("\x1b[B\x1b[B\r");
        ask_wait_editor(&mut pty);
        pty.input("check chinese again\x0aand second line");
        pty.input("\r");
        pty.wait_for("review shows custom answer", |screen| {
            screen.contents().contains("custom: check chinese again")
        });
        pty.input("\r");

        // The next request carries the exact answers for the original call id: questions in original
        // order, labels verbatim (without the Recommended badge), multi-select in original option order
        // and custom keeping its newline.
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_tool_outputs(&body, protocol),
            vec![(
                "ask-review".to_owned(),
                json!({"ok":true,"answers":[
                    {"id":"storage","selected_options":["Postgres"],"custom_input":null},
                    {"id":"checks","selected_options":["unit","lint"],"custom_input":"check chinese again\nand second line"}
                ]})
            )],
            "protocol {protocol}"
        );
        ask_final_reply(request, protocol, "ask roundtrip done");
        pty.wait_for("answer card records the submitted answers", |screen| {
            let text = screen.contents();
            text.contains("ask roundtrip done")
                && text.contains("Storage · Which database should the project use?")
                && text.contains("󰐾 Postgres")
                && text.contains("󰄲 unit")
                && text.contains("󰄲 lint")
                && text.contains("Other: check chinese again")
                && !text.contains("Other (type your own)")
        });
        pty.ready();
        pty.input("\x03");
        pty.finish();
        no_more(&server);
}

// --- Verification items: free-form answer with empty options + explicit submission of empty multi-select ---

// --- Verification items: malformed arguments open no panel and do not hang; normal tool error ---

include!("ask_tui/lifecycle_cases.rs");
