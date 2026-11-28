// /resume integration regression: reasoning and tool replay for both protocols, native history continuation,
// persistent input memory not being rewritten by replay, and keeping the current directory, status line, file
// candidates, model and thinking level when loading history from another directory.
// The log is appended in place and old tools are not re-executed; behavior is observed through the real
// terminal, HTTP and the file system.

fn resume_history_journal_dir(home: &Path) -> std::path::PathBuf {
    home.join(".pp/agent/sessions")
}

fn resume_history_session_files(home: &Path) -> Vec<std::path::PathBuf> {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(resume_history_journal_dir(home))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();
    files
}

// Deterministic listing order without sleeping: fixed, distinct mtimes.
fn resume_history_touch(path: &Path, seconds: u64) {
    let time = std::time::UNIX_EPOCH + Duration::from_secs(seconds);
    let file = std::fs::File::options().read(true).open(path).unwrap();
    file.set_times(
        std::fs::FileTimes::new()
            .set_modified(time)
            .set_accessed(time),
    )
    .unwrap();
}

// Sends one paging key and reports whether the visible screen changed, so
// callers can detect the top/bottom of the transcript without UI internals.
fn resume_history_page(pty: &mut Pty, key: &str) -> bool {
    let before = pty.parser.screen().contents();
    pty.input(key);
    let deadline = Instant::now() + Duration::from_millis(300);
    loop {
        pty.drain();
        if pty.parser.screen().contents() != before {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

// Pages the transcript from top to bottom and returns the markers sorted by
// their first on-screen (page, row, column) position, proving display order
// through the real terminal only.
fn resume_history_collect_order(pty: &mut Pty, markers: &[&str]) -> Vec<String> {
    for _ in 0..80 {
        if !resume_history_page(pty, "\x1b[5~") {
            break;
        }
    }
    let mut found: Vec<(String, usize, usize, usize)> = Vec::new();
    for page in 0..80 {
        let rows: Vec<String> = pty
            .parser
            .screen()
            .contents()
            .lines()
            .map(str::to_owned)
            .collect();
        for (row, line) in rows.iter().enumerate() {
            for marker in markers {
                if line.contains(marker) && !found.iter().any(|(seen, _, _, _)| seen == marker) {
                    let column = line.find(marker).unwrap();
                    found.push(((*marker).to_owned(), page, row, column));
                }
            }
        }
        if found.len() == markers.len() {
            break;
        }
        if !resume_history_page(pty, "\x1b[6~") {
            break;
        }
    }
    assert_eq!(
        found.len(),
        markers.len(),
        "transcript paging did not surface every marker; found {:?} of {:?}\n{}",
        found
            .iter()
            .map(|(marker, _, _, _)| marker.clone())
            .collect::<Vec<_>>(),
        markers,
        pty.diagnostics()
    );
    found.sort_by_key(|(_, page, row, column)| (*page, *row, *column));
    found.into_iter().map(|(marker, _, _, _)| marker).collect()
}

// A successful restore is confirmed by the visible conversation, the closed welcome page and a ready input.
fn resume_history_wait_resumed(pty: &mut Pty, _id: &str, replayed: &[&str]) {
    pty.wait_for(
        "the restored conversation is visible without a regular hint",
        |screen| {
            let text = screen.contents();
            input_is_ready(screen)
                && !text.contains("Resumed session:")
                && !text.contains("Resume session (")
                && replayed.iter().all(|expected| text.contains(expected))
                && !text.contains("█▀▀█")
        },
    );
}

// Read the native history the runtime already committed after exiting, plus the last retained main screen.
fn resume_history_exit_scrollback(pty: &mut Pty, last_answer: &str) -> String {
    pty.input("\x03");
    pty.wait_for("input mode is restored after exit", |screen| !screen.bracketed_paste());
    let (contents, _) = pty.native_history();
    assert!(contents.contains(last_answer), "{contents}");
    contents
}

// First reply of the saved session: visible reasoning plus a bash+read tool
// batch. The Responses variant carries opaque encrypted reasoning that must
// survive resume verbatim in later native requests.
fn resume_history_tool_batch(
    request: CapturedRequest,
    protocol: &str,
    reasoning: &str,
    calls: Value,
) {
    if protocol == "completion" {
        reply(
            request,
            json!({
                "role": "assistant",
                "content": null,
                "reasoning_content": reasoning,
                "tool_calls": calls
            }),
        );
        return;
    }
    let batch = json!([{
        "type": "reasoning",
        "id": "rh-think",
        "status": "completed",
        "summary": [{"type": "summary_text", "text": reasoning}],
        "encrypted_content": "opaque-resume-history"
    }])
    .as_array()
    .unwrap()
    .iter()
    .cloned()
    .chain(calls.as_array().unwrap().iter().cloned())
    .collect::<Vec<_>>();
    let batch = Value::Array(batch);
    let mut stream = request.start_sse().unwrap();
    stream.event(&json!({
        "type": "response.reasoning_summary_text.delta", "sequence_number": 0,
        "item_id": "rh-think", "output_index": 0, "summary_index": 0, "delta": reasoning
    })).unwrap();
    for (index, item) in batch.as_array().unwrap().iter().enumerate() {
        stream.event(&json!({
            "type": "response.output_item.done", "sequence_number": index + 1,
            "output_index": index, "item": item
        })).unwrap();
    }
    stream.event(&json!({
        "type": "response.completed",
        "sequence_number": batch.as_array().unwrap().len() + 1,
        "response": response_body(batch)
    })).unwrap();
    stream.finish();
}

#[test]
fn tui_slash_resume_history_replays_reasoning_tools_and_appends_native_context() {
    let protocol = "response";
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            r#"{"default_model":"fixture/primary","notification_timeout_seconds":1}"#,
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        let path = api_path(protocol);
        let command = "printf 'resume-history evidence\\n' >> side-effect.txt; \
for i in {1..60}; do printf 'resume-history-long-%03d\\n' \"$i\"; done > long.txt; \
for i in {1..60}; do printf 'resume-history-bash-%03d\\n' \"$i\"; done";

        // Phase 1: create a full session in the project directory — user
        // turn, streamed reasoning, a bash+read batch with a long read
        // result, then two more text turns.
        {
            let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
            pty.ready();
            pty.input("resume-history U1 ask\r");
            let (request, _) = pty.request_at(&server, path);
            let calls = if protocol == "completion" {
                json!([
                    call("rh-b", "bash", json!({"command": command})),
                    call("rh-r", "read", json!({"path": "long.txt"}))
                ])
            } else {
                json!([
                    response_call("rh-b", "bash", json!({"command": command})),
                    response_call("rh-r", "read", json!({"path": "long.txt"}))
                ])
            };
            resume_history_tool_batch(request, protocol, "resume-history think", calls);
            let (request, body) = pty.request_at(&server, path);
            if protocol == "completion" {
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(result(&messages[3], "rh-b")["exit_code"], 0);
                assert!(
                    result(&messages[4], "rh-r")["content"]
                        .as_str()
                        .unwrap()
                        .contains("resume-history-long-060")
                );
            } else {
                let input = body["input"].as_array().unwrap();
                assert_eq!(response_result(&input[4], "rh-b")["exit_code"], 0);
                assert!(
                    response_result(&input[5], "rh-r")["content"]
                        .as_str()
                        .unwrap()
                        .contains("resume-history-long-060")
                );
            }
            if protocol == "completion" {
                final_reply(request, "resume-history A1");
            } else {
                response_final(request, "resume-history A1");
            }
            pty.text("resume-history A1");
            pty.completed();
            pty.input("resume-history U2 ask\r");
            let (request, _) = pty.request_at(&server, path);
            if protocol == "completion" {
                final_reply(request, "resume-history A2");
            } else {
                response_final(request, "resume-history A2");
            }
            pty.text("resume-history A2");
            pty.completed();
            pty.input("/quit\r");
            pty.finish();
        }
        no_more(&server);
        let id = saved_session_id(home.path());
        let journal = resume_history_journal_dir(home.path()).join(format!("{id}.jsonl"));
        let before = std::fs::read(&journal).unwrap();
        let side_effect = project.path().join("side-effect.txt");
        assert_eq!(
            std::fs::read_to_string(&side_effect).unwrap(),
            "resume-history evidence\n"
        );

        // Phase 2: reopen interactively and resume through /resume.
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("/resume\r");
        pty.wait_for("session panel listing the saved summary", |screen| {
            let text = screen.contents();
            text.contains("Resume session")
                && text.contains("resume-history U1 ask")
                && text.lines().any(|line| line.starts_with('>'))
        });
        pty.input("\r");
        resume_history_wait_resumed(&mut pty, &id, &["resume-history A2"]);
        // Selecting and resuming a session performs no model request, and the
        // old bash tool side effect was not re-run.
        no_more(&server);
        assert_eq!(
            std::fs::read_to_string(&side_effect).unwrap(),
            "resume-history evidence\n"
        );

        // Restore only replaces the visible transcript; replay no longer writes input recall, so ↑ bringing
        // back the old session's prompt is not asserted here (persistent recall is covered by the standalone
        // input history regression).

        pty.wait_for("resume notification fades before scrolling", |screen| {
            !screen.contents().contains("Resumed session:")
        });

        // The replayed transcript keeps the original order, observable by
        // paging the real terminal from top to bottom.
        let markers = [
            "resume-history U1 ask",
            "resume-history think",
            "print(\"resume-history evidence",
            "Read · long.txt",
            "resume-history A1",
            "resume-history U2 ask",
            "resume-history A2",
        ];
        let ordered = resume_history_collect_order(&mut pty, &markers);
        assert_eq!(
            ordered, markers,
            "replayed turns are out of order for {protocol}"
        );

        // Tool output is restored folded; Ctrl+O expands and collapses globally while keeping the input.
        for _ in 0..80 {
            pty.drain();
            if pty.parser.screen().contents().contains("print(\"resume-history evidence") {
                break;
            }
            if !resume_history_page(&mut pty, "\x1b[5~") {
                break;
            }
        }
        pty.text("print(\"resume-history evidence");
        assert!(
            !pty.parser
                .screen()
                .contents()
                .contains("resume-history-bash-001"),
            "the long bash result must replay folded\n{}",
            pty.diagnostics()
        );
        pty.input("\x0f");

        // Expanding increases the card height; browse to the start first instead of relying on the viewport
        // position after expansion.
        for _ in 0..80 {
            pty.drain();
            if pty.parser.screen().contents().contains("resume-history-bash-001") {
                break;
            }
            if !resume_history_page(&mut pty, "\x1b[5~") {
                break;
            }
        }
        pty.text("resume-history-bash-001");
        pty.input("\x0f");
        pty.wait_for("tool output folds again", |screen| {
            !screen.contents().contains("resume-history-bash-001")
        });

        // Return to the tail after inspecting older history; new submissions
        // intentionally do not reset a manually scrolled viewport.
        for _ in 0..80 {
            if !resume_history_page(&mut pty, "\x1b[6~") {
                break;
            }
        }

        // Native continuation: U3 carries the full original context — user
        // turns, call ids, tool results, and opaque Responses reasoning — in
        // the original order, with U3 last.
        pty.input("resume-history U3 ask\r");
        let (request, body) = pty.request_at(&server, path);
        if protocol == "completion" {
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 9, "{body}");
            slash_assert_message(&messages[1], "user", "resume-history U1 ask");
            let calls = messages[2]["tool_calls"].as_array().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0]["id"], "rh-b");
            assert_eq!(calls[0]["function"]["name"], "bash");
            assert_eq!(calls[1]["id"], "rh-r");
            assert_eq!(calls[1]["function"]["name"], "read");
            assert_eq!(result(&messages[3], "rh-b")["exit_code"], 0);
            assert!(
                result(&messages[4], "rh-r")["content"]
                    .as_str()
                    .unwrap()
                    .contains("resume-history-long-060")
            );
            slash_assert_message(&messages[5], "assistant", "resume-history A1");
            slash_assert_message(&messages[6], "user", "resume-history U2 ask");
            slash_assert_message(&messages[7], "assistant", "resume-history A2");
            slash_assert_message(&messages[8], "user", "resume-history U3 ask");
            final_reply(request, "resume-history A3");
        } else {
            let input = body["input"].as_array().unwrap();
            assert_eq!(input.len(), 10, "{body}");
            response_user(&input[0], "resume-history U1 ask");
            assert_eq!(input[1]["type"], "reasoning");
            assert_eq!(input[1]["id"], "rh-think");
            assert_eq!(input[1]["encrypted_content"], "opaque-resume-history");
            assert_eq!(input[1]["summary"][0]["text"], "resume-history think");
            assert_eq!(input[2]["type"], "function_call");
            assert_eq!(input[2]["call_id"], "rh-b");
            assert_eq!(input[2]["name"], "bash");
            assert_eq!(input[3]["type"], "function_call");
            assert_eq!(input[3]["call_id"], "rh-r");
            assert_eq!(input[3]["name"], "read");
            assert_eq!(input[4]["type"], "function_call_output");
            assert_eq!(input[5]["type"], "function_call_output");
            assert_eq!(response_result(&input[4], "rh-b")["exit_code"], 0);
            assert!(
                response_result(&input[5], "rh-r")["content"]
                    .as_str()
                    .unwrap()
                    .contains("resume-history-long-060")
            );
            slash_assert_message(&input[6], "assistant", "resume-history A1");
            response_user(&input[7], "resume-history U2 ask");
            slash_assert_message(&input[8], "assistant", "resume-history A2");
            response_user(&input[9], "resume-history U3 ask");
            assert_eq!(body["store"], false);
            assert!(body.get("previous_response_id").is_none());
            assert!(
                body["include"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item == "reasoning.encrypted_content")
            );
            response_final(request, "resume-history A3");
        }
        pty.text("resume-history A3");
        pty.ready();
        no_more(&server);

        // The exit scrollback carries the whole conversation exactly once and
        // in order, including the post-resume turn.
        let scrollback = resume_history_exit_scrollback(&mut pty, "resume-history A3");
        let mut previous = 0;
        for marker in [
            "resume-history U1 ask",
            "resume-history think",
            "print(\"resume-history evidence",
            "Read · long.txt",
            "resume-history A1",
            "resume-history U2 ask",
            "resume-history A2",
            "resume-history U3 ask",
            "resume-history A3",
        ] {
            assert_eq!(
                scrollback.matches(marker).count(),
                1,
                "{marker} must appear exactly once in the exit scrollback:\n{scrollback}"
            );
            let at = scrollback.find(marker).unwrap();
            assert!(
                at > previous,
                "{marker} is out of order in the exit scrollback:\n{scrollback}"
            );
            previous = at;
        }
        pty.finish();

        // U3/A3 appended to the same journal in place: no fork, no new file,
        // and the old tool side effect still ran exactly once.
        let after = std::fs::read(&journal).unwrap();
        assert!(
            after.starts_with(&before),
            "journal must be appended in place for {protocol}"
        );
        assert!(String::from_utf8_lossy(&after).contains("resume-history U3 ask"));
        assert!(String::from_utf8_lossy(&after).contains("resume-history A3"));
        assert_eq!(resume_history_session_files(home.path()).len(), 1);
        assert_eq!(
            std::fs::read_to_string(&side_effect).unwrap(),
            "resume-history evidence\n"
        );
}

// The four cross-protocol restore combinations are independent: saved protocol × current protocol.
fn resume_history_model_case(saved_protocol: &str, current_protocol: &str) {
    let server = server();
            let home = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let settings = config_path(home.path(), "settings.yml");
            let models = config_path(home.path(), "models.yml");
            std::fs::write(&settings,
                r#"{"default_model":"fixture/primary","think_level":"high","edit":"hashline","notification_timeout_seconds":1}"#).unwrap();
            std::fs::write(&models, json!({"providers": {"fixture": slash_provider(
                &server, "fixture", saved_protocol,
                json!({"primary": {"model":"vendor-old"}})
            )}}).to_string()).unwrap();

            // Save history containing tool calls and reasoning output, then delete the old model configuration.
            let mut saved = Pty::start_with_args(project.path(), home.path(), &[]);
            saved.ready_model("fixture/primary");
            saved.input("matrix old prompt\r");
            let (request, _) = saved.request_at(&server, api_path(saved_protocol));
            let calls = if saved_protocol == "completion" {
                json!([call("matrix-call", "bash", json!({"command":"printf matrix-output"}))])
            } else {
                json!([response_call("matrix-call", "bash", json!({"command":"printf matrix-output"}))])
            };
            resume_history_tool_batch(request, saved_protocol, "matrix reasoning", calls);
            let (request, _) = saved.request_at(&server, api_path(saved_protocol));
            if saved_protocol == "completion" {
                final_reply(request, "matrix old answer");
            } else {
                response_final(request, "matrix old answer");
            }
            saved.text("matrix old answer");
            saved.completed();
            saved.input("/quit\r");
            saved.finish();
            let id = saved_session_id(home.path());
            let journal = resume_history_journal_dir(home.path()).join(format!("{id}.jsonl"));
            let original = std::fs::read(&journal).unwrap();

            let current_models = json!({"providers": {"fixture": slash_provider(
                &server, "fixture", current_protocol, json!({
                    "primary": {"model":"vendor-default"},
                    "selected": {"model":"vendor-current"}})
            )}}).to_string();
            std::fs::write(&models, &current_models).unwrap();
            std::fs::write(&settings,
                r#"{"edit":"replace","notification_timeout_seconds":1}"#).unwrap();
            let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
            pty.ready_model("fixture/primary");
            pty.input("/model fixture/selected\r");
            pty.ready_model("fixture/selected");
            pty.input("/effort low\r");
            pty.ready_effort("low");
            // Restore does not read the on-disk configuration and must not revert the manually selected model to
            // the default one.
            std::fs::write(&models, "invalid configuration").unwrap();
            pty.input("/resume\r");
            pty.text("matrix old prompt");
            pty.input("\r");
            resume_history_wait_resumed(&mut pty, &id, &["matrix old answer"]);
            pty.ready_model("fixture/selected");
            pty.ready_effort("low");
            assert_eq!(std::fs::read(&journal).unwrap(), original);
            no_more(&server);

            pty.input("matrix new prompt\r");
            let (request, body) = pty.request_at(&server, api_path(current_protocol));
            assert_eq!(body["model"], "vendor-current");
            if current_protocol == "completion" {
                assert_eq!(body["reasoning_effort"], "low");
            } else {
                assert_eq!(body["reasoning"]["effort"], "low");
            }
            let text = body.to_string();
            for expected in ["matrix old prompt", "matrix old answer", "matrix-call", "matrix-output", "matrix new prompt", "oldText"] {
                assert!(text.contains(expected), "{saved_protocol} -> {current_protocol}: {body}");
            }
            assert!(!text.contains("opaque-resume-history"), "{body}");
            if current_protocol == "completion" {
                final_reply(request, "matrix new answer");
            } else {
                response_final(request, "matrix new answer");
            }
            pty.text("matrix new answer");
            pty.completed();
            pty.input("/quit\r");
            pty.finish();

            // Load it again after a restart to verify the converted full state and later appends have no protocol
            // or cursor misalignment.
            // The manual selection lives only in memory (slot a) and is dropped on restart; the restored model
            // returns to the configuration default (slot b).
            std::fs::write(&models, &current_models).unwrap();
            let mut reloaded = Pty::start_with_args(project.path(), home.path(), &["--resume", &id]);
            reloaded.ready_model("fixture/primary");
            reloaded.input("matrix third prompt\r");
            let (request, body) = reloaded.request_at(&server, api_path(current_protocol));
            assert_eq!(body["model"], "vendor-default");
            for expected in ["matrix old prompt", "matrix old answer", "matrix-output", "matrix new prompt", "matrix new answer", "matrix third prompt"] {
                assert!(body.to_string().contains(expected), "{body}");
            }
            if current_protocol == "completion" {
                final_reply(request, "matrix third answer");
            } else {
                response_final(request, "matrix third answer");
            }
            reloaded.text("matrix third answer");
            reloaded.ready();
            reloaded.input("/quit\r");
            reloaded.finish();
            no_more(&server);
            assert_eq!(resume_history_session_files(home.path()).len(), 1);
}

#[test]
fn tui_slash_resume_keeps_selected_model_from_completion_to_response() {
    resume_history_model_case("completion", "response");
}

