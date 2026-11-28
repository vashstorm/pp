use super::*;

// Both providers share the model id `flash`: when the provider is omitted, favorites win within the same tier, and
// the wire name, key and level must follow the provider that is actually selected rather than the first one in the
// catalog.
fn flash_catalog(server: &TestServer, fixture_thinking: Value) -> String {
    let mut fixture = json!({"model": "fixture-flash"});
    if !fixture_thinking.is_null() {
        fixture["thinking"] = fixture_thinking;
    }
    json!({"providers": {
        "fixture": slash_provider(server, "fixture", "completion", json!({"flash": fixture})),
        "gateway": slash_provider(server, "gateway", "completion", json!({"flash": {
            "model": "gateway-flash",
            "thinking": {"efforts": ["low", "high"], "defaultLevel": "low"}
        }}))
    }})
    .to_string()
}

// The old settings fields are imported into agent.db during the startup migration; they seed the favorites without
// reading personal preferences.
fn favorite_gateway_settings() -> String {
    format!("{}favorite_models: [gateway/flash]\n", fixture_settings())
}

fn flash_home(server: &TestServer, thinking: Value) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        favorite_gateway_settings(),
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        flash_catalog(server, thinking),
    )
    .unwrap();
    install_command(home.path(), "commit.md", "COMMIT-MARKER $1");
    home
}

// The single request of the startup command: the favorite provider's wire name and key, the explicit level and the
// expanded template.
fn assert_gateway_commit_request(request: &CapturedRequest, body: &Value) {
    assert_eq!(body["model"], "gateway-flash");
    assert_eq!(authorization(request), "Bearer gateway-key");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    let prompt = slash_message_text(&messages[1]);
    assert!(prompt.contains("COMMIT-MARKER"), "{prompt}");
}

#[test]
fn model_arg_selects_favorite_wire_model_and_effort_for_initial_command() {
    let server = server();
    let home = flash_home(&server, Value::Null);
    let cli = Cli::spawn(home.path(), &["--model", "flash:high", "/commit"], &[]);
    let (request, body) = receive(&server);
    assert_gateway_commit_request(&request, &body);
    assert_eq!(body["reasoning_effort"], "high");
    final_reply(request, "commit fixture reply");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("Model: gateway/flash (effort high)"),
        "{stderr}"
    );
    assert_eq!(output.stdout, b"commit fixture reply\n");
    no_more(&server);
}
