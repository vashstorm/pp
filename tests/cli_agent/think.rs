use super::*;

fn assert_level(body: &Value, protocol: &str, level: &str) {
    let field = if protocol == "response" {
        "reasoning"
    } else {
        "reasoning_effort"
    };
    let other = if protocol == "response" {
        "reasoning_effort"
    } else {
        "reasoning"
    };
    assert!(body.get(other).is_none());
    if level == "default" {
        assert!(body.get(field).is_none());
    } else if protocol == "response" {
        assert_eq!(body[field], json!({"effort":level}));
    } else {
        assert_eq!(body[field], level);
    }
}

fn finish_request(request: Request, protocol: &str) {
    if protocol == "response" {
        response_final(request, "ok");
    } else {
        final_reply(request, "ok");
    }
}

#[test]
fn think_level_settings_reach_both_apis_including_default_omission() {
    for protocol in ["completion", "response"] {
        for level in ["default", "low", "medium", "high", "xhigh", "max"] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let settings = json!({"think_level":level}).to_string();
            let models = response_models(&server, Some(protocol));
            let cli = Cli::start_with(dir.path(), Some("test"), &settings, &models);
            let path = if protocol == "response" {
                "/v1/responses"
            } else {
                "/v1/chat/completions"
            };
            let (request, body) = receive_at(&server, path);
            assert_level(&body, protocol, level);
            finish_request(request, protocol);
            let output = cli.finish();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"ok\n");
        }
    }
}

#[test]
fn think_commands_preserve_history_and_level_across_model_switch_and_new() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers":[
        slash_provider(&server, "a", "completion", json!([{"id":"m","model":"a"}])),
        slash_provider(&server, "b", "response", json!([{"id":"m","model":"b"}]))
    ]})
    .to_string();
    let settings = r#"{"think_level":"low"}"#;
    let mut cli = Cli::start_with(dir.path(), None, settings, &models);
    cli.input("first\n");
    let (request, body) = receive(&server);
    assert_level(&body, "completion", "low");
    final_reply(request, "first answer");
    cli.input("/effort max\n/effort\n/effort invalid\n/model b/m\nsecond\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    assert_level(&body, "response", "max");
    let history = body["input"].as_array().unwrap();
    assert_eq!(history.len(), 3);
    slash_assert_message(&history[0], "user", "first");
    slash_assert_message(&history[1], "assistant", "first answer");
    response_final(request, "second answer");
    cli.input("/new\n/model a/m\nthird\n");
    let (request, body) = receive(&server);
    assert_level(&body, "completion", "max");
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    final_reply(request, "third answer");
    cli.input("/effort default\nfourth\n");
    let (request, body) = receive(&server);
    assert_level(&body, "completion", "default");
    final_reply(request, "fourth answer");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        b"first answer\nsecond answer\nthird answer\nfourth answer\n"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("thinking level must be one of"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("settings.jsonc")).unwrap(),
        settings
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("models.jsonc")).unwrap(),
        models
    );
}
