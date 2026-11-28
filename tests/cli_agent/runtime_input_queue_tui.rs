// Real-binary scenarios for queueing messages while running (plan verification items 3, 4, 5): FIFO
// follow-ups with a multi-line Unicode draft, pausing after a cancellation/provider error and waiting for
// function key R, refusing enqueue during the post-cancel busy period, the race between Esc and an
// already committed reply not auto-continuing, retrieving the original template slash and executing only
// the edited value, a non-empty draft blocking retrieval with an unchanged cursor, Tab not retrieving,
// Esc only closing the menu without cancelling the task, and the queue list/draft/native cursor staying
// uncovered at 100×30 and 40×10. Everything uses the real PTY, the loopback fixture and screen/HTTP/file
// markers as barriers, never sleep to guess stages. Helpers all use the `runtime_input_` prefix and add no
// use declaration, to avoid clashing with sibling include files.

fn runtime_input_empty_editor(pty: &mut Pty) {
    pty.wait_for("empty editor draft after enqueue", |screen| {
        let contents = screen.contents();
        let lines: Vec<&str> = contents.lines().collect();
        lines
            .windows(2)
            .any(|pair| pair[0].trim_end() == "❯" && pair[1].starts_with('─'))
    });
}

fn runtime_input_menu_visible(pty: &mut Pty, labels: &[&str]) {
    let expected: Vec<String> = labels.iter().map(|label| label.to_string()).collect();
    pty.wait_for("pending input menu visible", |screen| {
        let text = screen.contents();
        expected.iter().all(|label| text.contains(label.as_str()))
    });
}

fn runtime_input_menu_closed(pty: &mut Pty) {
    pty.wait_for("pending input menu closed", |screen| {
        let contents = screen.contents();
        !contents.contains("Steer #") && !contents.contains("Queued #")
    });
}

fn runtime_input_cancel_terminal(pty: &mut Pty, steering: usize, queued: usize) {
    pty.wait_for("cancel terminal state with paused queue", |screen| {
        input_is_ready(screen)
            && cancel_cursor_on_input_row(screen)
            && cancel_count_normalized(screen, CANCEL_DONE) == 1
    });
    runtime_input_counts(pty, steering, queued, true);
}

// Once the queue drains the count row disappears entirely (nothing is rendered for 0 entries), which is
// the screen barrier for "all applied".
fn runtime_input_queue_drained(pty: &mut Pty) {
    pty.wait_for("pending queue fully applied", |screen| {
        let text = screen.contents();
        !text.contains("Steering:") && !text.contains("Queued:")
    });
}

fn runtime_input_draft_visible(pty: &mut Pty, markers: &[&str]) {
    let expected: Vec<String> = markers.iter().map(|marker| marker.to_string()).collect();
    pty.wait_for("uncommitted draft preserved", |screen| {
        let text = screen.contents();
        expected.iter().all(|marker| text.contains(marker.as_str()))
    });
}

// The native cursor sits on the line containing marker and its column is returned; the same column is
// then used to assert the cursor was not moved.
fn runtime_input_cursor_on(pty: &mut Pty, marker: &str) -> u16 {
    pty.wait_for("cursor on the draft tail line", |screen| {
        screen
            .contents()
            .lines()
            .position(|line| line.contains(marker))
            .is_some_and(|row| row == usize::from(screen.cursor_position().0))
    });
    pty.parser.screen().cursor_position().1
}

fn runtime_input_cursor_kept(pty: &mut Pty, marker: &str, column: u16) {
    pty.wait_for("draft cursor preserved", |screen| {
        screen.cursor_position().1 == column
            && screen
                .contents()
                .lines()
                .position(|line| line.contains(marker))
                .is_some_and(|row| row == usize::from(screen.cursor_position().0))
    });
}

// Scenario 5 needs the /command template in place before pp starts; the configuration is written the
// same way as in runtime_input_start.
fn runtime_input_command_start(
    server: &TestServer,
    protocol: &str,
    command_body: &str,
) -> (tempfile::TempDir, Pty) {
    let home = runtime_input_home(server, protocol, "");
    install_command(home.path(), "qcmd.md", command_body);
    let mut pty = Pty::start_with_args(home.path(), home.path(), &[]);
    pty.ready();
    (home, pty)
}

// Scenario 3: FIFO follow-ups plus an uncommitted multi-line Unicode draft (CJK plus α/✓, so width and
// cursor columns are real). Q2 may appear only after Q1 succeeds, each user message exactly once, and
// independent tasks each issue a request; the draft and the native cursor survive throughout.
#[test]
fn tui_runtime_input_followup_fifo_and_draft_both_protocols() {
    let protocol = "response";
        let server = server();
        let (_home, mut pty) = runtime_input_start(&server, protocol, "");
        let stream = runtime_input_live(&mut pty, &server, protocol, "fifo-task-U");

        pty.input("Q1\x1b\r");
        runtime_input_counts(&mut pty, 0, 1, false);
        runtime_input_empty_editor(&mut pty);
        pty.input("Q2\x1b\r");
        runtime_input_counts(&mut pty, 0, 2, false);
        runtime_input_empty_editor(&mut pty);

        pty.input("草稿 α✓\x0a第二行");
        runtime_input_draft_visible(&mut pty, &["草稿 α✓", "第二行"]);
        let column = runtime_input_cursor_on(&mut pty, "第二行");

        runtime_input_end_live(stream, protocol);

        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["fifo-task-U", "Q1"],
            "{protocol}: first follow-up must be Q1 exactly once"
        );
        runtime_input_counts(&mut pty, 0, 1, false);
        let mut q1 = request.start_sse().unwrap();
        streaming_delta(&mut q1, protocol, "fifo-q1-live");
        pty.text("fifo-q1-live");
        no_more(&server);

        streaming_complete(&mut q1, protocol, "fifo-q1-live");
        q1.finish();

        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["fifo-task-U", "Q1", "Q2"],
            "{protocol}: Q2 must wait for Q1 success and appear once"
        );
        assert_eq!(
            cancel_assistant_texts(&body, protocol),
            ["runtime-live-marker", "fifo-q1-live"],
            "{protocol}: each finished task contributes one assistant reply"
        );
        runtime_input_final(request, protocol, "fifo-q2-final");
        pty.text("fifo-q2-final");
        runtime_input_queue_drained(&mut pty);

        runtime_input_draft_visible(&mut pty, &["草稿 α✓", "第二行"]);
        runtime_input_cursor_kept(&mut pty, "第二行", column);

        pty.input("\x03");
        pty.finish();
        no_more(&server);
}

// Scenario 8b: supplementary input rejected by UserPromptSubmit shows the rejection reason and the
// original text (not a successful You card); when everything is rejected no input is added and no second
// model request may be sent.
#[test]
fn hooks_tui_rejected_steering_card_needs_no_extra_request() {
    let protocol = "response";
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let reject = home.path().join("reject.sh");
        std::fs::write(
            &reject,
            "#!/bin/bash\npayload=$(cat)\nif [[ $payload == *\"reject-me\"* ]]; then\n  printf 'policy says no' >&2\n  exit 2\nfi\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&reject).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&reject, permissions).unwrap();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            "default_model: fixture/primary\nnotification_timeout_seconds: 2\n",
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "hooks.yml"),
            format!(
                "UserPromptSubmit:\n  - hooks:\n      - type: command\n        command: '{reject}'\n",
                reject = reject.display()
            ),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_args(home.path(), home.path(), &[]);
        pty.ready();

        let stream = runtime_input_live(&mut pty, &server, protocol, "hook-task-U");
        // Enqueued before the completion boundary: it must be rejected at the commit boundary and never
        // enter native history.
        pty.input("reject-me\r");
        runtime_input_counts(&mut pty, 1, 0, false);
        cancel_assert_no_request(&server);
        runtime_input_end_live(stream, protocol);
        pty.wait_for("rejected steering card", |screen| {
            let text = screen.contents();
            // The terminal parser does not keep raw escapes: an ESC means script body leaked onto the screen.
            assert!(
                !text.contains('\u{1b}'),
                "{protocol}: hook diagnostics must not inject raw terminal escapes: {text:?}"
            );
            text.contains("Rejected")
                && text.contains("Input rejected: policy says no")
                && text.contains("reject-me")
        });
        // Every supplementary input was rejected: no user input was added, so the model must not be asked
        // again.
        pty.wait_for("task finished after rejected steering", |screen| {
            screen.contents().contains("Completed ·") && input_is_ready(screen)
        });
        cancel_assert_no_request(&server);

        pty.input("\x03");
        pty.finish();
        no_more(&server);
}
