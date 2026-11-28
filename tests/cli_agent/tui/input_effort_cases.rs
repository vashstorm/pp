use super::*;

#[test]
fn tui_effort_and_shift_tab_apply_to_each_continuation_while_busy() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("draft.txt"), "draft body").unwrap();
    std::fs::write(dir.path().join("draft2.txt"), "second body").unwrap();
    let mut pty = Pty::start(&server, dir.path());
    pty.ready();
    pty.input("/effort ");
    pty.text("Highest level the model supports");
    pty.draft("/effort");
    assert!(server.try_recv().unwrap().is_none());
    pty.input("\u{1b}[B\u{1b}[B\u{1b}[B\u{1b}[B\r");
    pty.ready_effort("xhigh");
    pty.input("first\r");
    let (request, body) = pty.request(&server);
    assert_eq!(body["reasoning_effort"], "xhigh");
    // Do not reply to this round: an `/effort` submitted while running only registers the expected level, and the next request must
    // already pick up the new level at the boundary, while the status bar still shows the applied xhi before it.
    pty.input("/effort none\r");
    pty.wait_for(
        "submitting /effort while running registers a pending level",
        |screen| {
            let text = screen.contents();
            text.contains("pending next request")
                && text
                    .lines()
                    .last()
                    .is_some_and(|line| line.contains(" xhi ·"))
        },
    );
    calls_reply(
        request,
        json!([call("effort-one", "read", json!({"path":"draft.txt"}))]),
    );
    let (request, body) = pty.request(&server);
    assert_eq!(body["reasoning_effort"], "none");
    pty.wait_for(
        "the status bar switches to no think after the boundary",
        |screen| {
            input_is_ready(screen)
                && screen
                    .contents()
                    .lines()
                    .last()
                    .is_some_and(|line| line.contains(" off ·"))
        },
    );
    // Shift + Tab registers only the expected level too, while the same task's next round is streaming.
    pty.input("\u{1b}[Z");
    pty.wait_for(
        "Shift + Tab while running registers a pending level",
        |screen| screen.contents().contains("pending next request"),
    );
    calls_reply(
        request,
        json!([call("effort-two", "read", json!({"path":"draft2.txt"}))]),
    );
    let (request, body) = pty.request(&server);
    assert_eq!(body["reasoning_effort"], "low");
    final_reply(request, "busy effort done");
    pty.completed_after("busy effort done");
    pty.ready_effort("low");
    // The menu's "current" marker follows the applied level.
    pty.input("/effort ");
    pty.text("Light thinking (current)");
    pty.input("\x01\x0b");
    pty.draft("");
    pty.input("/effort max\r");
    pty.ready_effort("max");
    pty.input("\u{3}");
    pty.finish();
}
