// `pp --model … /COMMAND`: a single file command is submitted at startup and the session stays interactive after the
// task completes.
#[test]
fn initial_command_runs_on_startup_and_keeps_session_interactive() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(config_path(home.path(), "settings.yml"), fixture_settings()).unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();
    install_command(home.path(), "commit.md", "COMMIT-MARKER $1");
    let mut pty = Pty::start_with_args(
        home.path(),
        home.path(),
        &["--model", "fixture/primary", "/commit"],
    );

    // The startup command takes the same path as submitting with Enter: the request may only carry the expanded
    // template body.
    let (request, body) = pty.request(&server);
    assert_eq!(body["model"], "fixture-model");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    let prompt = slash_message_text(&messages[1]);
    assert!(prompt.contains("COMMIT-MARKER"), "{prompt}");
    final_reply(request, "COMMIT-DONE");
    pty.wait_for("startup command template marker and reply", |screen| {
        let text = screen.contents();
        text.contains("COMMIT-MARKER") && text.contains("COMMIT-DONE")
    });
    // The input box is still usable after the command finished: the session did not exit with the startup command.
    pty.completed_after("COMMIT-DONE");
    assert!(input_is_ready(pty.parser.screen()));
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
}

