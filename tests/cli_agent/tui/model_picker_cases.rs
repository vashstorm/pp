use super::*;

#[test]
fn tui_slash_model_picker_preserves_history() {
    let a = server();
    let b = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers": {
            "a": slash_provider(&a, "a", "completion", json!({
                "primary": {"model":"vendor-a"}})),
            "gateway": slash_provider(&b, "gateway", "completion", json!({
                "team/secondary": {"model":"vendor-b"}}))}})
    .to_string();
    std::fs::write(
        config_path(dir.path(), "settings.yml"),
        r#"{"default_model":"a/primary"}"#,
    )
    .unwrap();
    std::fs::write(config_path(dir.path(), "models.yml"), models).unwrap();
    let mut pty = Pty::start_with_existing_files(dir.path());
    pty.ready_model("a/primary");
    pty.input("remember picker alpha\r");
    let (request, body) = pty.request(&a);
    assert_eq!(body["model"], "vendor-a");
    let system = body["messages"][0].clone();
    final_reply(request, "picker alpha retained");
    pty.text("picker alpha retained");
    pty.completed();
    pty.ready_model("a/primary");

    // Pin the model command with a unique prefix; under fuzzy matching "/m" also hits /compact.
    pty.input("/mo");
    pty.draft("/mo");
    pty.resize(4, 20);
    pty.resize(30, 100);
    pty.input("\t");
    pty.draft("/model");
    no_more(&a);
    no_more(&b);
    pty.input(" ");
    pty.wait_for(
        "a space expands the model menu above the input box",
        |screen| {
            let text = screen.contents();
            let (cursor_row, cursor_col) = screen.cursor_position();
            text.contains("a/primary")
                && text.contains("gateway/team/secondary")
                && !text.contains("vendor-b")
                && !text.contains("[completion]")
                && text.find('').unwrap() < text.find("a/primary").unwrap()
                && !text.contains("❯ /model")
                && text.contains("a/primary  ")
                && !text.contains("(current)")
                && !text.contains("(default)")
                && !screen.hide_cursor()
                && cursor_col == 2
                && text
                    .lines()
                    .nth(usize::from(cursor_row))
                    .is_some_and(|line| line.starts_with(''))
        },
    );
    // Before confirming, exercise head/tail navigation, paging and a window resize.
    pty.input("\x1b[H\x1b[5~\x1b[A\x1b[F\x1b[6~\x1b[B");
    pty.resize(4, 20);
    pty.resize(30, 100);
    pty.text("gateway/team/secondary");
    pty.input("\r");
    pty.ready_model("gateway/team/secondary");
    pty.text("picker alpha retained");
    no_more(&a);
    no_more(&b);

    pty.input("recall picker alpha\r");
    let (request, body) = pty.request(&b);
    assert_eq!(authorization(&request), "Bearer gateway-key");
    assert_eq!(body["model"], "vendor-b");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0], system);
    slash_assert_message(&messages[1], "user", "remember picker alpha");
    slash_assert_message(&messages[2], "assistant", "picker alpha retained");
    slash_assert_message(&messages[3], "user", "recall picker alpha");
    let history = messages.clone();
    final_reply(request, "picker beta retained");
    pty.text("picker beta retained");
    pty.completed();
    pty.ready_model("gateway/team/secondary");

    // The current model is selected by default, so confirming directly must not pick the first item by mistake.
    pty.input("/model\r");
    pty.text("a/primary");
    pty.input("\r");
    pty.wait_for("current model confirmation closes picker", |screen| {
        let text = screen.contents();
        input_is_ready(screen)
            && text
                .lines()
                .last()
                .is_some_and(|line| line.contains("gateway/team/secondary"))
            && !text.contains("a/primary")
    });
    pty.draft("");
    no_more(&a);
    no_more(&b);

    pty.input("/model\r");
    pty.text("a/primary");
    pty.input("\x1b[A\x1b");
    pty.draft("");
    pty.ready_model("gateway/team/secondary");
    no_more(&a);
    no_more(&b);
    pty.input("/model a/primary\r");
    pty.ready_model("a/primary");
    pty.input("direct model recall\r");
    let (request, body) = pty.request(&a);
    assert_eq!(authorization(&request), "Bearer a-key");
    assert_eq!(body["model"], "vendor-a");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(&messages[..history.len()], history.as_slice());
    slash_assert_message(&messages[4], "assistant", "picker beta retained");
    slash_assert_message(&messages[5], "user", "direct model recall");
    assert_eq!(messages.len(), 6);
    final_reply(request, "direct history retained");
    pty.text("direct history retained");
    pty.completed();
    pty.ready_model("a/primary");

    pty.input("/bogus\r");
    pty.draft("/bogus");
    assert!(!pty.parser.screen().contents().contains("unknown command"));
    no_more(&a);
    no_more(&b);
    pty.input("\x7f\x7f\x7f\x7f\x7f\x7f");
    pty.input("\x1b[200~/not-a-command\nordinary pasted task\x1b[201~");
    pty.draft("/not-a-command");
    pty.draft("ordinary pasted task");
    no_more(&a);
    no_more(&b);
    pty.input("\r");
    let (request, body) = pty.request(&a);
    let messages = body["messages"].as_array().unwrap();
    slash_assert_message(
        messages.last().unwrap(),
        "user",
        "/not-a-command\nordinary pasted task",
    );
    assert_eq!(messages.len(), 8);
    final_reply(request, "multiline slash task accepted");
    pty.text("multiline slash task accepted");
    // The model command requires the task to settle, so seeing the body alone is no processing fence.
    pty.completed();
    pty.ready_model("a/primary");

    pty.input("/model\r");
    pty.text("gateway/team/secondary");
    // The menu must not intercept Ctrl-C or hinder terminal restoration.
    pty.input("\x03");
    pty.finish();
    no_more(&a);
    no_more(&b);
}
