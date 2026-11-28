use super::*;

#[test]
fn tui_session_resume_replays_history_without_request() {
    let protocol = "response";
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let id = create_tool_session(&server, home.path(), protocol);
    let path = api_path(protocol);

    let mut pty = Pty::start_with_args(home.path(), home.path(), &["-r", &id]);
    pty.wait_for(
        "the restored conversation shows no routine notice",
        |screen| {
            let text = screen.contents();
            text.contains("remember tui-marker")
                && text.contains("tui-remembered")
                && !text.contains("Resumed session:")
        },
    );
    // No welcome page; tool cards replay the saved exit state without faking a wall duration.
    let text = pty.parser.screen().contents();
    assert!(!text.contains("▄▄▄██▀▀██"));
    assert!(text.contains("print(\"tool-out\")"));
    assert!(text.contains("Read · tui-read.txt"));
    assert!(text.contains("tool-out"));
    assert!(text.contains("Timeout: 60s"));
    assert!(!text.contains("Exit:"));
    assert!(!text.contains("Wall:"));
    // The input box is empty and history browsing is not active.
    pty.ready();
    let prompt_line = pty
        .parser
        .screen()
        .contents()
        .lines()
        .find(|line| line.starts_with('❯'))
        .unwrap()
        .trim()
        .to_owned();
    assert_eq!(prompt_line, "❯");
    // The restore sends no request.
    no_more(&server);
    // Input history is restored: ↑ refills the old input directly.
    pty.input("\x1b[A");
    pty.draft("remember tui-marker");
    pty.input("\x1b[B");
    pty.draft("");

    // The new input keeps the original context, appends to the same file and keeps the ID.
    pty.input("what did I say\r");
    let (request, body) = pty.request_at(&server, path);
    if protocol == "completion" {
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 7);
        assert_eq!(messages[1]["content"], "remember tui-marker");
        assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(messages[2]["tool_calls"][1]["function"]["name"], "read");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[4]["role"], "tool");
        assert_eq!(messages[5]["content"], "tui-remembered");
        assert_eq!(messages[6]["content"], "what did I say");
    } else {
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 7);
        response_user(&input[0], "remember tui-marker");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "b1");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "r1");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[4]["type"], "function_call_output");
        slash_assert_message(&input[5], "assistant", "tui-remembered");
        response_user(&input[6], "what did I say");
    }
    if protocol == "completion" {
        final_reply(request, "you said tui-marker");
    } else {
        response_final(request, "you said tui-marker");
    }
    pty.text("you said tui-marker");
    pty.ready();
    // A restored session no longer reports a new Session ID again.
    assert!(!pty.parser.screen().contents().contains("Session:"));
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
    assert_eq!(saved_session_id(home.path()), id);
    let journal = std::fs::read_to_string(
        home.path()
            .join(".pp/agent/sessions")
            .join(format!("{id}.jsonl")),
    )
    .unwrap();
    assert!(journal.contains("what did I say"));

    // A forced termination may lose this round's uncommitted body, but the native history must still hold no failure prefix.
    let interrupted_home = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(interrupted_home.path(), "settings.yml"),
        fixture_settings(),
    )
    .unwrap();
    std::fs::write(
        config_path(interrupted_home.path(), "models.yml"),
        response_models(&server, Some(protocol)),
    )
    .unwrap();
    let mut interrupted = Pty::start_with_existing_files(interrupted_home.path());
    interrupted.ready();
    interrupted.input("interrupted input\r");
    let (request, _) = receive_at(&server, path);
    let mut stream = request.start_sse().unwrap();
    streaming_delta(&mut stream, protocol, "partial-session-marker");
    // Observed terminal output fences event persistence; do not finish
    // the stream before killing the process.
    interrupted.text("partial-session-marker");
    drop(interrupted);
    drop(stream);
    let interrupted_id = saved_session_id(interrupted_home.path());
    let interrupted_path = interrupted_home
        .path()
        .join(".pp/agent/sessions")
        .join(format!("{interrupted_id}.jsonl"));
    let before = std::fs::read(&interrupted_path).unwrap();
    let saved = records(&interrupted_path);
    assert!(
        !saved
            .iter()
            .any(|entry| { entry["type"] == "message" && entry["message"]["role"] == "assistant" })
    );
    assert!(
        !saved.iter().any(|entry| {
            entry["type"] == "custom" && entry["customType"] == "pp.native_append"
        })
    );
    let mut resumed = Pty::start_with_args(
        interrupted_home.path(),
        interrupted_home.path(),
        &["-r", &interrupted_id],
    );

    resumed.wait_for(
        "resuming an interrupted session shows a red dialog",
        |screen| {
            notification_bounds(screen, "Error", vt100::Color::Rgb(224, 108, 117)).is_some()
                && screen
                    .contents()
                    .contains("Previous request was interrupted.")
        },
    );
    resumed.ready();
    let screen = resumed.parser.screen().contents();
    assert_eq!(screen.matches("interrupted input").count(), 1);
    assert_eq!(screen.matches("partial-session-marker").count(), 0);
    no_more(&server);
    resumed.input("/quit\r");
    resumed.finish();
    assert_eq!(std::fs::read(&interrupted_path).unwrap(), before);
    no_more(&server);
}
