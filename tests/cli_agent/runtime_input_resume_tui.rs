// Restore and compaction boundaries for input submitted while running (plan verification items 6, 7): real
// pp binary PTY scenarios.
// Scenario 6: after a task that applied Steering plus a real write side effect, produce a paused queue of
// unapplied messages and an uncommitted draft, then exit; -r restores only the applied history, one copy
// each, without restoring the in-memory queue, and the new task's request contains the old Steering exactly
// once without the unapplied messages or the draft, while the old write side effect is not repeated (the
// request only replays the old tool results). Scenario 7: Steering is queued while the server holds the
// automatic summary for an explicit context_window; the summary request does not contain it, while the
// ordinary request after the summary does and the tool batch still completes fully, with the write side
// effect really landing on disk. Everything uses the real terminal, loopback HTTP and file markers as
// barriers, never sleep to guess stages; helper names keep the runtime_input_ prefix and do not clash with
// sibling files.

#[test]
fn tui_runtime_input_resume_replays_applied_history_once_both_protocols() {
    let protocol = "response";
        let fixture = server();
        let (home, mut pty) = runtime_input_start(&fixture, protocol, "");
        // Task 1: queue S while U is running; after it finishes, S and U share one request and the model's write
        // lands on disk.
        let stream = runtime_input_live(&mut pty, &fixture, protocol, "resume-live-prompt");
        pty.input("resume-steer\r");
        runtime_input_counts(&mut pty, 1, 0, false);
        pty.draft("");
        runtime_input_end_live(stream, protocol);
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            vec!["resume-live-prompt".to_owned(), "resume-steer".to_owned()]
        );
        runtime_input_calls(
            request,
            protocol,
            &[(
                "resume-write-1",
                "write",
                json!({"path":"resume-proof.txt","content":"resume-ok\n"}),
            )],
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].0, "resume-write-1");
        runtime_input_final(request, protocol, "resume-task-answer");
        pty.text("resume-task-answer");
        pty.ready();
        assert_eq!(
            std::fs::read_to_string(home.path().join("resume-proof.txt")).unwrap(),
            "resume-ok\n"
        );
        // Task 2: Alt+Enter queues unapplied Q1 while running; after the Esc cancellation the queue is paused,
        // so exit with the draft kept.
        let stream = runtime_input_live(&mut pty, &fixture, protocol, "resume-cancel-prompt");
        pty.input("resume-queued\x1b\r");
        runtime_input_counts(&mut pty, 0, 1, false);
        pty.draft("");
        pty.input("\x1b");
        cancel_wait_cancelled_ready(&mut pty);
        drop(stream);
        runtime_input_counts(&mut pty, 0, 1, true);
        pty.input("resume-draft");
        pty.draft("resume-draft");
        pty.input("\x03");
        pty.finish();

        // A changed sentinel detects even an accidental replay of the same write.
        std::fs::write(
            home.path().join("resume-proof.txt"),
            "must-not-be-overwritten\n",
        )
        .unwrap();
        // -r restore: replay only the applied history, send no request, and do not restore the in-memory queue or
        // the draft.
        let id = saved_session_id(home.path());
        let mut resumed = Pty::start_with_args(home.path(), home.path(), &["-r", &id]);
        resume_history_wait_resumed(&mut resumed, &id, &[]);
        let replayed = resume_history_collect_order(
            &mut resumed,
            &[
                "resume-live-prompt",
                "resume-steer",
                "resume-task-answer",
                "resume-cancel-prompt",
            ],
        );
        assert_eq!(
            replayed,
            vec![
                "resume-live-prompt".to_owned(),
                "resume-steer".to_owned(),
                "resume-task-answer".to_owned(),
                "resume-cancel-prompt".to_owned(),
            ]
        );
        context_tui_page_to_bottom(&mut resumed);
        let visible = resumed.parser.screen().contents();
        assert!(!visible.contains("resume-queued"), "{visible}");
        assert!(!visible.contains("resume-draft"), "{visible}");
        assert!(!visible.contains("Steering:"), "{visible}");
        no_more(&fixture);

        // An explicit new task: native history contains S exactly once and neither the unapplied message nor the
        // draft appears; the old write result is only replayed, its side effect is not repeated and the file content
        // stays unchanged.
        resumed.input("resume-final-prompt\r");
        let (request, body) = resumed.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            vec![
                "resume-live-prompt".to_owned(),
                "resume-steer".to_owned(),
                "resume-cancel-prompt".to_owned(),
                "resume-final-prompt".to_owned(),
            ]
        );
        assert!(
            !serde_json::to_string(&body)
                .unwrap()
                .contains("resume-draft")
        );
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].0, "resume-write-1");
        runtime_input_final(request, protocol, "resume-final-answer");
        resumed.text("resume-final-answer");
        resumed.ready();
        no_more(&fixture);
        resumed.input("\x03");
        resumed.finish();
        assert_eq!(
            std::fs::read_to_string(home.path().join("resume-proof.txt")).unwrap(),
            "must-not-be-overwritten\n"
        );
}

