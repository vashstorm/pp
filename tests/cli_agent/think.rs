use super::*;

pub(super) fn assert_level(body: &Value, protocol: &str, level: &str) {
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
    let level = if level == "default" { "none" } else { level };
    if protocol == "response" {
        assert_eq!(body[field]["effort"], level);
        if level == "none" {
            assert!(body[field].get("summary").is_none());
        } else {
            assert_eq!(body[field]["summary"], "auto");
        }
    } else {
        assert_eq!(body[field], level);
    }
}

pub(super) fn finish_request(request: Request, protocol: &str) {
    if protocol == "response" {
        response_final(request, "ok");
    } else {
        final_reply(request, "ok");
    }
}

#[test]
fn think_level_settings_reach_both_apis_including_none_and_default_alias() {
    for protocol in ["completion", "response"] {
        for level in ["none", "default", "low", "medium", "high", "xhigh", "max"] {
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
        std::fs::read_to_string(config_path(dir.path(), "settings.jsonc")).unwrap(),
        settings
    );
    assert_eq!(
        std::fs::read_to_string(config_path(dir.path(), "models.jsonc")).unwrap(),
        models
    );
}

#[test]
fn think_level_runtime_changes_do_not_override_startup_settings_after_restart() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let settings = r#"{"think_level":"high"}"#;
        let models = response_models(&server, Some(protocol));
        let path = if protocol == "response" {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        };
        let mut cli = Cli::start_with(dir.path(), None, settings, &models);
        cli.input("first\n");
        let (request, body) = receive_at(&server, path);
        assert_level(&body, protocol, "high");
        finish_request(request, protocol);
        cli.input("/effort none\nsecond\n");
        let (request, body) = receive_at(&server, path);
        assert_level(&body, protocol, "none");
        finish_request(request, protocol);
        cli.input("/new\nthird\n");
        let (request, body) = receive_at(&server, path);
        assert_level(&body, protocol, "none");
        finish_request(request, protocol);
        cli.input("/quit\n");
        assert!(cli.finish().status.success());
        assert_eq!(
            std::fs::read_to_string(config_path(dir.path(), "settings.jsonc")).unwrap(),
            settings
        );

        let cli = Cli::start_with_existing_files(dir.path(), Some("after restart"));
        let (request, body) = receive_at(&server, path);
        assert_level(&body, protocol, "high");
        finish_request(request, protocol);
        assert!(cli.finish().status.success());
        no_more(&server);
    }
}

#[test]
fn think_level_missing_settings_or_field_explicitly_sends_none_to_both_apis() {
    for protocol in ["completion", "response"] {
        for settings in [None, Some("{}")] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            if let Some(settings) = settings {
                std::fs::write(config_path(dir.path(), "settings.jsonc"), settings).unwrap();
            }
            std::fs::write(
                config_path(dir.path(), "models.jsonc"),
                response_models(&server, Some(protocol)),
            )
            .unwrap();
            let cli = Cli::start_with_existing_files(dir.path(), Some("test"));
            let path = if protocol == "response" {
                "/v1/responses"
            } else {
                "/v1/chat/completions"
            };
            let (request, body) = receive_at(&server, path);
            assert_level(&body, protocol, "none");
            finish_request(request, protocol);
            assert!(cli.finish().status.success());
        }
    }
}
