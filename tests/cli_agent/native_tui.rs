// Wait for every body line to enter the native history or the current viewport, instead of treating one
// input echo as the history queue being drained.
// A resize reflows the parser and repaints the application at the same time, so a line crossing the 40-column
// wrap boundary may appear twice in the rebuilt native history; only the loss of a source line fails here,
// and "exactly once" is not used as a barrier.
fn native_wait_complete(pty: &mut Pty, prefix: &str, count: usize) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let history = pty.native_history().0;
        if (0..count).all(|index| history.contains(&format!("{prefix}{index:03}"))) { return; }
        assert!(Instant::now() < deadline, "native history was not fully committed: {history}");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn native_tui_resume_hides_cursor_until_history_rebuild_finishes_both_protocols() {
    let protocol = "response";
        let provider = server();
        let home = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            home.path(),
            Some("resume-cursor-task"),
            fixture_settings(),
            &response_models(&provider, Some(protocol)),
        );
        let (request, _) = receive_at(&provider, api_path(protocol));
        let answer = format!(
            "```text\n{}\n```",
            (0..180)
                .map(|i| format!("resume-cursor-{i:03} 中😀"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        diff_blocks_final(request, protocol, &answer);
        assert!(cli.finish().status.success());
        let id = saved_session_id(home.path());

        for startup in [true, false] {
            let args = if startup {
                vec!["-r", id.as_str()]
            } else {
                vec![]
            };
            let mut pty = Pty::start_with_args(home.path(), home.path(), &args);
            let before = if startup {
                0
            } else {
                pty.ready();
                pty.input("/resume\r");
                pty.wait_for("restore panel lists the target session", |screen| {
                    let text = screen.contents();
                    text.contains("resume-cursor-task")
                        && text.lines().any(|line| line.starts_with('>'))
                });
                let before = pty.raw.len();
                pty.input("\r");
                before
            };
            native_wait_complete(&mut pty, "resume-cursor-", 180);
            pty.wait_for("cursor returns to the input box after the history is rebuilt", cancel_cursor_on_input_row);
            let output = String::from_utf8_lossy(&pty.raw[before..]);
            let first = output.find("resume-cursor-000").expect("first history line");
            let last = output.rfind("resume-cursor-179").expect("last history line");
            let hide = output.find("\x1b[?25l").expect("cursor hidden before the rebuild");
            let show = output.find("\x1b[?25h").expect("cursor restored after the rebuild");
            assert!(
                hide < first && show > last,
                "the cursor was shown early during the history rebuild: {output:?}"
            );
            assert_eq!(
                output.matches("\x1b[?25h").count(),
                1,
                "history batches repeatedly show the cursor"
            );
            assert!(
                output.matches("\x1b[?2026h").count() > 2,
                "not all history batches were covered"
            );
            pty.input("resume-cursor-draft");
            pty.draft("resume-cursor-draft");
            pty.input("\x03");
            pty.finish();
            no_more(&provider);
        }
}

// Native history must be committed while running; exiting only wraps up and must not re-emit earlier sessions.

