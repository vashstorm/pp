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
        std::fs::read_to_string(config_path(dir.path(), "settings.yml")).unwrap(),
        settings
    );
    assert_eq!(
        std::fs::read_to_string(config_path(dir.path(), "models.yml")).unwrap(),
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
            std::fs::read_to_string(config_path(dir.path(), "settings.yml")).unwrap(),
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

fn thinking_model(id: &str, model: &str, efforts: Value, default_level: Option<&str>) -> Value {
    let mut entry = json!({"id": id, "model": model, "thinking": {"efforts": efforts}});
    if let Some(level) = default_level {
        entry["thinking"]["defaultLevel"] = json!(level);
    }
    entry
}

fn model_thinking_reply(request: Request, protocol: &str, text: &str) {
    if protocol == "response" {
        response_final(request, text);
    } else {
        final_reply(request, text);
    }
}

#[test]
fn model_thinking_startup_default_and_whitelist_enforced_for_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let path = if protocol == "response" {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        };
        let models = json!({"providers":[slash_provider(
            &server,
            "fixture",
            protocol,
            json!([thinking_model(
                "primary",
                "fixture-model",
                json!(["low", "high"]),
                Some("high")
            )])
        )]})
        .to_string();
        let mut cli = Cli::start_with(dir.path(), None, r#"{"think_level":"max"}"#, &models);
        // The explicit model default beats the global setting on startup.
        cli.input("first\n");
        let (request, body) = receive_at(&server, path);
        assert_eq!(body["model"], "fixture-model");
        assert_level(&body, protocol, "high");
        model_thinking_reply(request, protocol, "first answer");
        cli.input("/effort low\nsecond\n");
        let (request, body) = receive_at(&server, path);
        assert_level(&body, protocol, "low");
        model_thinking_reply(request, protocol, "second answer");
        // max, none and default are outside the whitelist: rejected without a
        // model task, so the next request is still the follow-up task at low.
        cli.input("/effort max\n/effort none\n/effort default\nthird\n");
        let (request, body) = receive_at(&server, path);
        assert_level(&body, protocol, "low");
        if protocol == "response" {
            let history = body["input"].as_array().unwrap();
            assert_eq!(history.len(), 5);
            slash_assert_message(&history[0], "user", "first");
            slash_assert_message(&history[1], "assistant", "first answer");
            slash_assert_message(&history[2], "user", "second");
            slash_assert_message(&history[3], "assistant", "second answer");
            slash_assert_message(&history[4], "user", "third");
        } else {
            let history = body["messages"].as_array().unwrap();
            assert_eq!(history.len(), 6);
            assert_eq!(history[0]["role"], "system");
            slash_assert_message(&history[1], "user", "first");
            slash_assert_message(&history[2], "assistant", "first answer");
            slash_assert_message(&history[3], "user", "second");
            slash_assert_message(&history[4], "assistant", "second answer");
            slash_assert_message(&history[5], "user", "third");
        }
        model_thinking_reply(request, protocol, "third answer");
        cli.input("/effort\n/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            output.stdout,
            b"first answer\nsecond answer\nthird answer\n"
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(
            stderr
                .matches("thinking level is not supported by the current model; options: low, high")
                .count(),
            3,
            "{stderr}"
        );
        assert_eq!(
            stderr
                .matches("Thinking level: low (options: low, high)")
                .count(),
            2,
            "{stderr}"
        );
        no_more(&server);
    }
}

#[test]
fn model_thinking_without_explicit_default_uses_global_then_lowest_allowed() {
    for protocol in ["completion", "response"] {
        let path = if protocol == "response" {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        };
        for (efforts, global, expected) in [
            (json!(["high", "low"]), "high", "high"),
            (json!(["xhigh", "medium"]), "max", "medium"),
        ] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let settings = json!({"think_level": global}).to_string();
            let models = json!({"providers":[slash_provider(
                &server,
                "fixture",
                protocol,
                json!([thinking_model("primary", "fixture-model", efforts, None)])
            )]})
            .to_string();
            let cli = Cli::start_with(dir.path(), Some("test"), &settings, &models);
            let (request, body) = receive_at(&server, path);
            assert_level(&body, protocol, expected);
            finish_request(request, protocol);
            let output = cli.finish();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"ok\n");
        }

        // An empty efforts list keeps every global level selectable.
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let models = json!({"providers":[slash_provider(
            &server,
            "fixture",
            protocol,
            json!([thinking_model("primary", "fixture-model", json!([]), None)])
        )]})
        .to_string();
        let mut cli = Cli::start_with(dir.path(), None, r#"{"think_level":"max"}"#, &models);
        cli.input("/effort\n/effort none\ntask\n");
        let (request, body) = receive_at(&server, path);
        assert_level(&body, protocol, "none");
        finish_request(request, protocol);
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"ok\n");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("Thinking level: max (options: none, low, medium, high, xhigh, max)"),
            "{stderr}"
        );
        no_more(&server);
    }
}

#[test]
fn model_thinking_switch_same_protocol_applies_destination_policy() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers":[
        slash_provider(&server, "a", "completion", json!([thinking_model(
            "m", "vendor-a", json!(["low", "high"]), Some("high")
        )])),
        slash_provider(&server, "b", "completion", json!([
            thinking_model("keep", "vendor-keep", json!(["low", "medium", "high"]), None),
            thinking_model("lowest", "vendor-lowest", json!(["xhigh", "medium"]), None)
        ])),
        slash_provider(&server, "c", "completion", json!([thinking_model(
            "override", "vendor-override", json!(["low", "medium", "high"]), Some("low")
        )])),
        slash_provider(&server, "d", "completion", json!([thinking_model(
            "global", "vendor-global", json!(["low", "max"]), None
        )]))
    ]})
    .to_string();
    let mut cli = Cli::start_with(dir.path(), None, r#"{"think_level":"max"}"#, &models);
    cli.input("first\n");
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "vendor-a");
    assert_level(&body, "completion", "high");
    final_reply(request, "first answer");
    // An explicit destination default wins even though the current level is
    // also supported by the destination.
    cli.input("/model c/override\nsecond\n");
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "vendor-override");
    assert_level(&body, "completion", "low");
    let history = body["messages"].as_array().unwrap();
    assert_eq!(history.len(), 4);
    slash_assert_message(history.last().unwrap(), "user", "second");
    final_reply(request, "second answer");
    // No explicit default and an unsupported current level: the canonical
    // lowest allowed level, regardless of the JSON array order.
    cli.input("/model b/lowest\nthird\n");
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "vendor-lowest");
    assert_level(&body, "completion", "medium");
    final_reply(request, "third answer");
    // No explicit default and a supported current level: retained.
    cli.input("/model b/keep\nfourth\n");
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "vendor-keep");
    assert_level(&body, "completion", "medium");
    final_reply(request, "fourth answer");
    // No explicit default, unsupported current level, allowed global: global.
    cli.input("/model d/global\nfifth\n");
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "vendor-global");
    assert_level(&body, "completion", "max");
    final_reply(request, "fifth answer");
    // Switching to the current reference is a no-op that keeps the selection.
    cli.input("/effort low\n/model d/global\nsixth\n");
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "vendor-global");
    assert_level(&body, "completion", "low");
    let history = body["messages"].as_array().unwrap();
    assert_eq!(history.len(), 12);
    slash_assert_message(history.last().unwrap(), "user", "sixth");
    final_reply(request, "sixth answer");
    // /new clears history but keeps the selected level.
    cli.input("/new\nseventh\n");
    let (request, body) = receive(&server);
    assert_level(&body, "completion", "low");
    let history = body["messages"].as_array().unwrap();
    assert_eq!(history.len(), 2);
    slash_assert_message(&history[1], "user", "seventh");
    final_reply(request, "seventh answer");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        b"first answer\nsecond answer\nthird answer\nfourth answer\nfifth answer\nsixth answer\nseventh answer\n"
    );
    no_more(&server);
}

#[test]
fn model_thinking_switch_cross_protocol_applies_destination_policy() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers":[
        slash_provider(&server, "a", "completion", json!([
            thinking_model("m", "vendor-a", json!(["low", "high"]), Some("high")),
            thinking_model("keep", "vendor-a-keep", json!(["low", "medium", "high"]), None)
        ])),
        slash_provider(&server, "b", "response", json!([
            thinking_model("override", "vendor-b-override", json!(["low", "medium", "high"]), Some("medium")),
            thinking_model("lowest", "vendor-b-lowest", json!(["xhigh", "low"]), None)
        ]))
    ]})
    .to_string();
    let mut cli = Cli::start_with(dir.path(), None, r#"{"think_level":"max"}"#, &models);
    cli.input("first\n");
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "vendor-a");
    assert_level(&body, "completion", "high");
    final_reply(request, "first answer");
    // Crossing protocols applies the destination default over the supported
    // current level and converts the history.
    cli.input("/model b/override\nsecond\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    assert_eq!(body["model"], "vendor-b-override");
    assert_level(&body, "response", "medium");
    let history = body["input"].as_array().unwrap();
    assert_eq!(history.len(), 3);
    slash_assert_message(&history[0], "user", "first");
    slash_assert_message(&history[1], "assistant", "first answer");
    slash_assert_message(&history[2], "user", "second");
    response_final(request, "second answer");
    // No explicit default and a supported current level: retained.
    cli.input("/model a/keep\nthird\n");
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "vendor-a-keep");
    assert_level(&body, "completion", "medium");
    let history = body["messages"].as_array().unwrap();
    assert_eq!(history.len(), 6);
    slash_assert_message(history.last().unwrap(), "user", "third");
    final_reply(request, "third answer");
    // No explicit default and an unsupported current level: canonical lowest.
    cli.input("/model b/lowest\nfourth\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    assert_eq!(body["model"], "vendor-b-lowest");
    assert_level(&body, "response", "low");
    response_final(request, "fourth answer");
    // Same-reference switching is a no-op that keeps the selected level.
    cli.input("/effort xhigh\n/model b/lowest\nfifth\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    assert_level(&body, "response", "xhigh");
    response_final(request, "fifth answer");
    // /new clears history but keeps the selected level.
    cli.input("/new\nsixth\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    assert_level(&body, "response", "xhigh");
    assert_eq!(body["input"].as_array().unwrap().len(), 1);
    response_final(request, "sixth answer");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        b"first answer\nsecond answer\nthird answer\nfourth answer\nfifth answer\nsixth answer\n"
    );
    no_more(&server);
}

#[test]
fn model_thinking_switch_responses_cross_provider_applies_destination_policy() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers":[
        slash_provider(&server, "a", "response", json!([
            thinking_model("m", "vendor-a", json!(["low", "high"]), Some("high")),
            thinking_model("keep", "vendor-a-keep", json!(["low", "medium", "high"]), None)
        ])),
        slash_provider(&server, "b", "response", json!([
            thinking_model("override", "vendor-b-override", json!(["low", "medium", "high"]), Some("medium")),
            thinking_model("lowest", "vendor-b-lowest", json!(["xhigh", "low"]), None)
        ]))
    ]})
    .to_string();
    let mut cli = Cli::start_with(dir.path(), None, r#"{"think_level":"max"}"#, &models);
    cli.input("first\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    assert_eq!(body["model"], "vendor-a");
    assert_level(&body, "response", "high");
    response_final(request, "first answer");
    // Crossing providers on the Responses protocol applies the destination
    // default over the supported current level and converts the history.
    cli.input("/model b/override\nsecond\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    assert_eq!(authorization(&request), "Bearer b-key");
    assert_eq!(body["model"], "vendor-b-override");
    assert_level(&body, "response", "medium");
    let history = body["input"].as_array().unwrap();
    assert_eq!(history.len(), 3);
    slash_assert_message(&history[0], "user", "first");
    slash_assert_message(&history[1], "assistant", "first answer");
    slash_assert_message(&history[2], "user", "second");
    response_final(request, "second answer");
    // No explicit default and a supported current level: retained.
    cli.input("/model a/keep\nthird\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    assert_eq!(authorization(&request), "Bearer a-key");
    assert_eq!(body["model"], "vendor-a-keep");
    assert_level(&body, "response", "medium");
    let history = body["input"].as_array().unwrap();
    assert_eq!(history.len(), 5);
    slash_assert_message(&history[1], "assistant", "first answer");
    slash_assert_message(history.last().unwrap(), "user", "third");
    response_final(request, "third answer");
    // No explicit default and an unsupported current level: canonical lowest.
    cli.input("/model b/lowest\nfourth\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    assert_eq!(body["model"], "vendor-b-lowest");
    assert_level(&body, "response", "low");
    response_final(request, "fourth answer");
    // Same-reference switching is a no-op that keeps the selected level.
    cli.input("/effort xhigh\n/model b/lowest\nfifth\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    assert_level(&body, "response", "xhigh");
    response_final(request, "fifth answer");
    // /new clears history but keeps the selected level.
    cli.input("/new\nsixth\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    assert_level(&body, "response", "xhigh");
    assert_eq!(body["input"].as_array().unwrap().len(), 1);
    response_final(request, "sixth answer");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        b"first answer\nsecond answer\nthird answer\nfourth answer\nfifth answer\nsixth answer\n"
    );
    no_more(&server);
}

#[test]
fn think_level_missing_settings_or_field_explicitly_sends_none_to_both_apis() {
    for protocol in ["completion", "response"] {
        for settings in [None, Some("{}")] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            if let Some(settings) = settings {
                std::fs::write(config_path(dir.path(), "settings.yml"), settings).unwrap();
            }
            std::fs::write(
                config_path(dir.path(), "models.yml"),
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
