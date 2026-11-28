// Coverage 1: both protocols run the before-headers and after-partial-delta phases. After an Esc cancel the old
// connection must really close before the next task is submitted; no implicit retry; draft/cursor/alternate screen
// preserved; the new request's native history holds both user messages exactly once and no half-finished assistant.
#[test]
fn tui_cancel_idle_headers_and_sse_close_connection_before_next_request_and_recover() {
    for (protocol, phase) in [("completion", "headers"), ("response", "sse")] {
            let raw = CancelRawModel::start();
            let home = tempfile::tempdir().unwrap();
            // The raw TCP fixture only observes the main requests; the separate title service stays pending and is cancelled with the task.
            let title_server = server();
            let mut settings: Value = serde_json::from_str(CANCEL_SETTINGS).unwrap();
            settings["model_roles"] = json!({"fast":{"model":"title/fast","effort":"none"}});
            let mut models: Value = serde_json::from_str(&raw.models(protocol)).unwrap();
            models["providers"]["title"] = slash_provider(
                &title_server,
                "title",
                protocol,
                json!({"fast":{"model":"title-model"}}),
            );
            std::fs::write(
                config_path(home.path(), "settings.yml"),
                settings.to_string(),
            )
            .unwrap();
            std::fs::write(config_path(home.path(), "models.yml"), models.to_string()).unwrap();
            let mut pty = Pty::start_with_existing_files(home.path());
            pty.ready();
            pty.input("cancel idle prompt\r");
            let (path, body) = raw.expect_request();
            assert_eq!(path, api_path(protocol));
            let first_body = body.clone();
            assert_eq!(
                cancel_user_messages(&body, protocol),
                ["cancel idle prompt"]
            );
            if phase == "sse" {
                raw.delta(protocol, "partial-cancel-token");
                pty.text("partial-cancel-token");
            } else {
                raw.hold();
            }
            pty.input("kept draft");
            pty.draft("kept draft");
            pty.input("\x1b");
            cancel_wait_cancelled_ready(&mut pty);
            if phase == "sse" {
                assert_eq!(
                    cancel_count_normalized(pty.parser.screen(), "partial-cancel-token"),
                    1,
                    "a partial that was already shown must be preserved"
                );
            }
            // The old connection is already closed before the next task is submitted instead of waiting for the next block_on.
            raw.expect_close();
            // Without sending a new user message first there are no extra model requests.
            raw.assert_silent();
            // The cursor stays at the end of the draft: appending one character proves both draft and cursor survived.
            pty.input("!");
            pty.draft("kept draft!");
            pty.input("\r");
            let (path, body) = raw.expect_request();
            assert_eq!(path, api_path(protocol));
            let text = serde_json::to_string(&body).unwrap();
            assert!(!text.contains("pp.cancelled"), "pp.cancelled must not reach the request");
            assert!(
                !text.contains("partial-cancel-token"),
                "a half-finished assistant must not enter the native history"
            );
            if protocol == "completion" {
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(
                    messages
                        .iter()
                        .filter(|item| item["role"] == "user")
                        .count(),
                    2
                );
                assert_eq!(
                    messages
                        .iter()
                        .filter(|item| item["role"] == "assistant")
                        .count(),
                    0
                );
                assert_eq!(messages[1]["content"], "cancel idle prompt");
                assert_eq!(messages[2]["content"], "kept draft!");
            } else {
                let items = body["input"].as_array().unwrap();
                assert_eq!(items.len(), 2);
                response_user(&items[0], "cancel idle prompt");
                response_user(&items[1], "kept draft!");
            }
            // The new runtime/client really works: the next request returns a normal final answer.
            raw.final_answer(protocol, "idle recovered answer");
            pty.text("idle recovered answer");
            pty.ready();
            pty.input("/quit\r");
            pty.finish();
            raw.assert_silent();
            let database = super::stats::database(home.path());
            let statuses: Vec<String> = database
                .prepare("SELECT status FROM requests WHERE purpose!='task_title' ORDER BY id")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(statuses, ["aborted", "ok"]);
            let contexts = super::stats::contexts(home.path());
            let captured: Vec<_> = contexts
                .iter()
                .filter_map(|context| context.get("request"))
                .collect();
            assert_eq!(captured, [&first_body]);
            let invalid_titles: i64 = database.query_row("SELECT COUNT(*) FROM requests WHERE purpose='task_title' AND status!='aborted'", [], |r| r.get(0)).unwrap();
            assert_eq!(invalid_titles, 0);
            let ttft: Option<f64> = database
                .query_row(
                    "SELECT ttft_ms FROM requests WHERE status='aborted' AND purpose!='task_title'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(ttft.is_some(), phase == "sse");
    }
}

// Coverage 2: write→bash→write batch cancel on both protocols. The parent shell is reaped even though it ignores TERM,
// kept files and captured output are not rolled back, skipped calls never execute, and the next request pairs all three
// tool results with their original call_ids in order without re-running them; the cancel itself triggers no HTTP.
#[test]
fn tui_cancel_tool_batch_stops_process_group_and_pairs_results_on_next_request() {
    let protocol = "response";
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), CANCEL_SETTINGS).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        cancel_drive_batch(
            &mut pty,
            &fixture,
            protocol,
            "cancel batch prompt",
            project.path(),
        );
        pty.input("post-batch draft");
        pty.draft("post-batch draft");
        pty.input("\x1b");
        cancel_wait_cancelled_ready(&mut pty);
        cancel_assert_batch_stopped(project.path());
        // Settling the cancel triggers no model request.
        cancel_assert_no_request(&fixture);
        // Submit the next task explicitly (the kept draft) and verify the pairing without replay.
        pty.input("\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        cancel_assert_paired_results(&body, protocol);
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["cancel batch prompt", "post-batch draft"]
        );
        let text = serde_json::to_string(&body).unwrap();
        assert!(!text.contains("pp.cancelled"), "pp.cancelled must not reach the request");
        if protocol == "completion" {
            final_reply(request, "batch recovered answer");
        } else {
            response_final(request, "batch recovered answer");
        }
        pty.text("batch recovered answer");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        cancel_assert_no_request(&fixture);
        // The old tools were never replayed: file state stays as it was after the cancel.
        assert_eq!(
            std::fs::read_to_string(project.path().join("kept.txt")).unwrap(),
            "kept"
        );
        assert!(!project.path().join("skipped.txt").exists());
}

// Coverage 4: persistence and restart for all three cancels — before headers, SSE idle, and tool batch. A cancel only
// shows the status line, appends no notice block, and never claims interrupted by mistake; /resume lists it as aborted and
// may turn done after a new task completes; the native requests of -r ID -p and -c ID contain only accepted user messages,
// the complete old history and paired tool results, never execute old tools, and pp.cancelled does not reach the request.
#[derive(Clone, Copy, PartialEq)]
enum CancelPhase {
    Headers,
    Sse,
    Tools,
}

// One full round trip per cancel phase × protocol: the cancel terminal state is persisted, -r resumes, /resume marks it,
// and line mode -r/-c reuses the history. Phases are independent and split into separate cases that run in parallel.
fn cancel_terminal_state_case(protocol: &str, phase: CancelPhase) {
    let fixture = server();
            let home = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            std::fs::write(config_path(home.path(), "settings.yml"), CANCEL_SETTINGS).unwrap();
            std::fs::write(
                config_path(home.path(), "models.yml"),
                response_models(&fixture, Some(protocol)),
            )
            .unwrap();
            let path = api_path(protocol);
            let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
            pty.ready();
            let mut pid_before = Vec::new();
            let mut child_before = Vec::new();
            match phase {
                CancelPhase::Headers => {
                    pty.input("persist cancel prompt\r");
                    // The request arrived but was not answered; holding CapturedRequest keeps the peer from EOFing early.
                    let (_request, _) = pty.request_at(&fixture, path);
                    pty.input("\x1b");
                    cancel_wait_cancelled_ready(&mut pty);
                }
                CancelPhase::Sse => {
                    pty.input("persist cancel prompt\r");
                    let (request, _) = pty.request_at(&fixture, path);
                    let mut stream = request.start_sse().unwrap();
                    streaming_delta(&mut stream, protocol, "persist-partial-token");
                    pty.text("persist-partial-token");
                    pty.input("\x1b");
                    cancel_wait_cancelled_ready(&mut pty);
                }
                CancelPhase::Tools => {
                    cancel_drive_batch(
                        &mut pty,
                        &fixture,
                        protocol,
                        "persist cancel prompt",
                        project.path(),
                    );
                    pty.input("\x1b");
                    cancel_wait_cancelled_ready(&mut pty);
                    cancel_assert_batch_stopped(project.path());
                    pid_before = std::fs::read(project.path().join("bash.pid")).unwrap();
                    child_before = std::fs::read(project.path().join("bash-child.pid")).unwrap();
                }
            }
            if phase == CancelPhase::Sse {
                assert_eq!(
                    cancel_count_normalized(pty.parser.screen(), "persist-partial-token"),
                    1
                );
            }
            pty.input("/quit\r");
            pty.finish();
            cancel_assert_no_request(&fixture);

            let id = saved_session_id(home.path());
            let journal = std::fs::read_to_string(
                home.path()
                    .join(".pp/agent/sessions")
                    .join(format!("{id}.jsonl")),
            )
            .unwrap();
            assert!(journal.contains("pp.cancelled"), "the cancel terminal state must be persisted");

            // Interactive -r resume: replay the accepted user message, append no notice block, and show no
            // bogus interrupted banner.
            let mut resumed = Pty::start_with_args(project.path(), home.path(), &["-r", &id]);
            resumed.wait_for("resuming a cancelled session: accepted user message and idle input", |screen| {
                input_is_ready(screen) && screen.contents().contains("persist cancel prompt")
            });
            assert!(
                !cancel_normalize(resumed.parser.screen())
                    .contains("Previousrequestwasinterrupted."),
                "an explicit cancel must not be reported as interrupted"
            );
            assert_eq!(
                cancel_count_normalized(resumed.parser.screen(), "persist cancel prompt"),
                1
            );
            if phase == CancelPhase::Sse {
                assert_eq!(
                    cancel_count_normalized(resumed.parser.screen(), "persist-partial-token"),
                    1
                );
            }
            resumed.ready();
            if phase == CancelPhase::Tools {
                // Resume does not replay old tools: the PID markers and files stay unchanged.
                assert_eq!(
                    std::fs::read(project.path().join("bash.pid")).unwrap(),
                    pid_before
                );
                assert_eq!(
                    std::fs::read(project.path().join("bash-child.pid")).unwrap(),
                    child_before
                );
                assert_eq!(
                    std::fs::read_to_string(project.path().join("kept.txt")).unwrap(),
                    "kept"
                );
                assert!(!project.path().join("skipped.txt").exists());
            }
            cancel_assert_no_request(&fixture);

            // /resume lists it as aborted rather than done; the result is installed asynchronously by the history worker.
            resume_boundary_open(&mut resumed);
            resumed.wait_for("the resume list marks the cancelled session aborted", |screen| {
                screen.contents().contains("■ aborted")
            });
            assert!(!resumed.parser.screen().contents().contains("\u{f00c} done"));
            resume_boundary_close(&mut resumed);

            // After the cancel terminal state, completing a new task lets the list show done again.
            resumed.input("persist recovery prompt\r");
            let (request, _) = resumed.request_at(&fixture, path);
            if protocol == "completion" {
                final_reply(request, "persist recovery answer");
            } else {
                response_final(request, "persist recovery answer");
            }
            resumed.text("persist recovery answer");
            // The input box is editable while running; idle commands only run once this task completes.
            resumed.completed();
            resume_boundary_open(&mut resumed);
            resumed.wait_for("the list marks done after a new task completes", |screen| {
                screen.contents().contains("\u{f00c} done")
            });
            resume_boundary_close(&mut resumed);
            resumed.input("/quit\r");
            resumed.finish();
            cancel_assert_no_request(&fixture);

            // Line mode -r ID -p: the request holds only accepted user messages plus the complete old history, no pp.cancelled.
            let home_path = home.path().to_str().unwrap();
            let cli = Cli::spawn(
                project.path(),
                &["-r", id.as_str(), "-p", "persist cli follow"],
                &[("HOME", home_path)],
            );
            let (request, body) = receive_at(&fixture, path);
            assert_eq!(
                cancel_user_messages(&body, protocol),
                [
                    "persist cancel prompt",
                    "persist recovery prompt",
                    "persist cli follow"
                ]
            );
            assert_eq!(
                cancel_assistant_texts(&body, protocol),
                ["persist recovery answer"]
            );
            let text = serde_json::to_string(&body).unwrap();
            assert!(!text.contains("pp.cancelled"), "pp.cancelled must not reach the request");
            assert!(
                !text.contains("persist-partial-token"),
                "a half-finished assistant must not enter the native history"
            );
            if phase == CancelPhase::Tools {
                cancel_assert_paired_results(&body, protocol);
            } else {
                assert!(
                    cancel_tool_outputs(&body, protocol).is_empty(),
                    "a cancel outside a tool batch must not produce tool results"
                );
            }
            if protocol == "completion" {
                final_reply(request, "persist cli answer");
            } else {
                response_final(request, "persist cli answer");
            }
            let output = cli.finish();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"persist cli answer\n");
            cancel_assert_no_request(&fixture);

            // Line mode -c ID: send the saved native request; the history stays valid and old tools do not run.
            let cli = Cli::spawn(project.path(), &["-c", id.as_str()], &[("HOME", home_path)]);
            let (request, body) = receive_at(&fixture, path);
            assert_eq!(
                cancel_user_messages(&body, protocol),
                [
                    "persist cancel prompt",
                    "persist recovery prompt",
                    "persist cli follow"
                ]
            );
            assert_eq!(
                cancel_assistant_texts(&body, protocol),
                ["persist recovery answer", "persist cli answer"]
            );
            let text = serde_json::to_string(&body).unwrap();
            assert!(!text.contains("pp.cancelled"), "pp.cancelled must not reach the request");
            assert!(!text.contains("persist-partial-token"));
            if phase == CancelPhase::Tools {
                cancel_assert_paired_results(&body, protocol);
            }
            if protocol == "completion" {
                final_reply(request, "persist continue answer");
            } else {
                response_final(request, "persist continue answer");
            }
            let output = cli.finish();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"persist continue answer\n");
            cancel_assert_no_request(&fixture);
        if phase == CancelPhase::Tools {
            assert_eq!(
                std::fs::read_to_string(project.path().join("kept.txt")).unwrap(),
                "kept"
            );
            assert!(!project.path().join("skipped.txt").exists());
        }
}

// Persistence failure mode of coverage 4: when the cancel settle's commit/sync fails, the UI must show
// persisting session first, never claim the cancel succeeded, write no pp.cancelled, send no further model
// request, and still stop the real bash process on the cancel path.
#[test]
fn tui_cancel_journal_failure_reports_persisting_session_not_success() {
    let protocol = "response";
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), CANCEL_SETTINGS).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        // ulimit -f 256: the header/user records still fit, while the large streamed content makes the cancel
        // settle's native commit fail with EFBIG.
        let mut pty = Pty::start_with_file_limit(project.path(), home.path(), 256, &[]);
        pty.ready();
        pty.input("overflow cancel batch\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let big = "A".repeat(400_000);
        let command =
            "sh -c 'printf \"overflow-ready\\n\"; printf \"%s\" \"$$\" > overflow-bash.pid; sleep 30'";
        if protocol == "completion" {
            reply(
                request,
                json!({
                    "role":"assistant", "content":big,
                    "tool_calls":[call("overflow-bash", "bash", json!({"command":command}))]
                }),
            );
        } else {
            response_reply(
                request,
                json!([
                    response_message("msg-overflow", json!([response_text(&big)])),
                    response_call("overflow-bash", "bash", json!({"command":command}))
                ]),
            );
        }
        // The PID marker is the fence for entering the bash execution phase.
        cancel_wait_pid_files(project.path(), &["overflow-bash.pid"]);
        pty.input("\x1b");
        pty.wait_for(
            "a failed cancel commit shows the persisting session red dialog",
            |screen| {
                notification_bounds(screen, "Error", vt100::Color::Rgb(224, 108, 117)).is_some()
                    && cancel_normalize(screen).contains("persistingsession")
            },
        );
        // No claim that the cancel succeeded.
        assert_eq!(
            cancel_count_normalized(pty.parser.screen(), CANCEL_DONE),
            0,
            "a persistence failure must not show the cancel completion status line"
        );
        pty.ready();
        // No further model request after the failed cancel settle.
        cancel_assert_no_request(&fixture);
        // The cancel path still stopped the real bash process.
        cancel_wait_pid_dead(&project.path().join("overflow-bash.pid"));
        pty.input("\x03");
        pty.finish();
        let id = saved_session_id(home.path());
        let journal = std::fs::read_to_string(
            home.path()
                .join(".pp/agent/sessions")
                .join(format!("{id}.jsonl")),
        )
        .unwrap();
        assert!(
            !journal.contains("pp.cancelled"),
            "a failed cancel terminal-state commit must not write pp.cancelled"
        );
}

