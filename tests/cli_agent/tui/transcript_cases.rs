use super::*;

#[test]
fn tui_ctrl_c_keeps_partial_output_without_waiting_for_response() {
    let protocol = "response";
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(config_path(dir.path(), "settings.yml"), fixture_settings()).unwrap();
    std::fs::write(
        config_path(dir.path(), "models.yml"),
        response_models(&server, Some(protocol)),
    )
    .unwrap();
    let mut pty = Pty::start_with_existing_files(dir.path());
    pty.ready();
    pty.input("pending-task\r");
    let path = if protocol == "completion" {
        "/v1/chat/completions"
    } else {
        "/v1/responses"
    };
    let (request, _) = receive_at(&server, path);
    let mut stream = request.start_sse().unwrap();
    streaming_delta(&mut stream, protocol, "partial-retained 中😀");
    pty.text("partial-retained");
    pty.input("\x03");
    pty.wait_for("the unfinished streaming output survives exit", |screen| {
        !screen.alternate_screen()
            && screen.contents().contains("partial-retained 中😀")
            && background_color(screen, "pending-task") == USER_BACKGROUND
            && background_color(screen, "partial-retained") == ASSISTANT_BACKGROUND
    });
    pty.finish();
    stream.finish();
    no_more(&server);
}
