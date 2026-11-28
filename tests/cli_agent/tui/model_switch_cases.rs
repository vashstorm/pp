use super::*;

#[test]
fn tui_model_switch_safe_boundary_and_failure() {
    let a = server();
    let b = server();
    let bad = server();
    let dir = tempfile::tempdir().unwrap();
    let mut broken = slash_provider(
        &bad,
        "bad",
        "response",
        json!({"broken":{"model":"vendor-broken"}}),
    );
    broken["api_key"] = json!("tui-secret-marker\ninvalid-header");
    let models = json!({"providers": {
        "a": slash_provider(&a, "a", "completion", json!({
            "primary": {"model":"vendor-a"}})),
        "gateway": slash_provider(&b, "gateway", "completion", json!({
            "team/secondary": {"model":"vendor-b"}})),
        "bad": broken
    }})
    .to_string();
    std::fs::write(
        config_path(dir.path(), "settings.yml"),
        r#"{"default_model":"a/primary"}"#,
    )
    .unwrap();
    std::fs::write(config_path(dir.path(), "models.yml"), models).unwrap();
    let mut pty = Pty::start_with_existing_files(dir.path());
    pty.ready_model("a/primary");
    pty.input("hold busy history\r");
    let (pending, first) = pty.request(&a);
    pty.input("/models gateway/team/secondary\r");
    pty.text("pending safe boundary");
    pty.input("draft-中");
    pty.draft("draft-中");
    no_more(&a);
    no_more(&b);
    no_more(&bad);
    final_reply(pending, "busy history retained");
    pty.text("busy history retained");
    pty.completed();
    pty.ready_model("gateway/team/secondary");
    pty.draft("draft-中");
    no_more(&a);
    no_more(&b);
    pty.input("\x01\x0b");
    pty.input("after busy switch\r");
    let (request, body) = pty.request(&b);
    assert_eq!(authorization(&request), "Bearer gateway-key");
    assert_eq!(body["model"], "vendor-b");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(&messages[..2], first["messages"].as_array().unwrap());
    slash_assert_message(&messages[2], "assistant", "busy history retained");
    slash_assert_message(&messages[3], "user", "after busy switch");
    assert_eq!(messages.len(), 4);
    let history = messages.clone();
    final_reply(request, "busy switch retained");
    pty.text("busy switch retained");
    pty.completed();
    pty.ready_model("gateway/team/secondary");

    pty.input("/model bad/broken\r");
    pty.wait_for("failed switch is a red local error popup", |screen| {
        notification_bounds(screen, "Error", vt100::Color::Rgb(224, 108, 117)).is_some()
            && screen.contents().to_lowercase().contains("header")
    });
    pty.ready_model("gateway/team/secondary");
    no_more(&a);
    no_more(&b);
    no_more(&bad);
    pty.input("after failed switch\r");
    let (request, body) = pty.request(&b);
    assert_eq!(authorization(&request), "Bearer gateway-key");
    assert_eq!(body["model"], "vendor-b");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(&messages[..history.len()], history.as_slice());
    slash_assert_message(&messages[4], "assistant", "busy switch retained");
    slash_assert_message(&messages[5], "user", "after failed switch");
    assert_eq!(messages.len(), 6);
    final_reply(request, "failed switch history retained");
    pty.text("failed switch history retained");
    pty.ready_model("gateway/team/secondary");
    for secret in [
        "tui-secret-marker",
        "invalid-header",
        "a-key",
        "gateway-key",
    ] {
        assert!(
            !String::from_utf8_lossy(&pty.raw).contains(secret),
            "TUI leaked {secret}"
        );
    }

    pty.input("exit with model request pending\r");
    let (pending, body) = pty.request(&b);
    assert_eq!(body["model"], "vendor-b");
    slash_assert_message(
        body["messages"].as_array().unwrap().last().unwrap(),
        "user",
        "exit with model request pending",
    );
    pty.input("/quit\r");
    pty.finish();
    drop(pending);
    no_more(&a);
    no_more(&b);
    no_more(&bad);
}
