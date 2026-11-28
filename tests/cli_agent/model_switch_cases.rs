use super::*;

#[test]
fn model_switch_completion_response_completion_preserves_full_tool_batches() {
    let a = server();
    let b = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers": {
        "a": slash_provider(&a, "a", "completion", json!({"primary": {"model":"vendor-a"}})),
        "b": slash_provider(&b, "b", "response", json!({"secondary": {"model":"vendor-b"}}))}})
    .to_string();
    let mut cli = Cli::start_with(
        dir.path(),
        None,
        r#"{"default_model":"a/primary"}"#,
        &models,
    );
    cli.input("completion first task\n");
    let (request, body) = receive(&a);
    let system = body["messages"][0].clone();
    let calls = json!([
        call(
            "completion-write",
            "write",
            json!({"path":"completion.txt","content":"alpha"})
        ),
        call("completion-read", "read", json!({"path":"completion.txt"}))
    ]);
    let assistant = json!({
        "role":"assistant", "content":"completion commentary", "tool_calls":calls
    });
    reply(request, assistant.clone());
    let (request, body) = receive(&a);
    assert_eq!(body["messages"][2], assistant);
    assert_eq!(
        result(&body["messages"][3], "completion-write")["bytes_written"],
        5
    );
    assert_eq!(result(&body["messages"][4], "completion-read")["ok"], true);
    let completion_history = body["messages"].as_array().unwrap().clone();
    final_reply(request, "completion final");
    std::fs::write(dir.path().join("completion.txt"), "completion sentinel").unwrap();

    cli.input("/model b/secondary\nresponse second task\n");
    let (request, body) = receive_at(&b, "/v1/responses");
    assert_eq!(authorization(&request), "Bearer b-key");
    assert_eq!(body["model"], "vendor-b");
    assert_eq!(body["instructions"], system["content"]);
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 8);
    slash_assert_portable(input);
    response_user(&input[0], "completion first task");
    slash_assert_message(&input[1], "assistant", "completion commentary");
    slash_assert_response_calls(&input[2..4], &calls);
    for (index, old_index, id) in [(4, 3, "completion-write"), (5, 4, "completion-read")] {
        assert_eq!(
            input[index]["output"],
            completion_history[old_index]["content"]
        );
        assert_eq!(
            response_result(&input[index], id),
            result(&completion_history[old_index], id)
        );
    }
    slash_assert_message(&input[6], "assistant", "completion final");
    response_user(&input[7], "response second task");
    let second_initial = input.clone();
    let batch = slash_response_batch();
    response_reply(request, batch.clone());
    let (request, body) = receive_at(&b, "/v1/responses");
    let input = body["input"].as_array().unwrap();
    assert_eq!(&input[..8], second_initial.as_slice());
    assert_eq!(&input[8..13], batch.as_array().unwrap().as_slice());
    assert_eq!(
        response_result(&input[13], "response-write")["bytes_written"],
        4
    );
    assert_eq!(response_result(&input[14], "response-read")["ok"], true);
    let response_results = [input[13].clone(), input[14].clone()];
    response_final(request, "response final");
    std::fs::write(dir.path().join("response.txt"), "response sentinel").unwrap();

    cli.input("/model a/primary\ncompletion third task\n");
    let (request, body) = receive(&a);
    assert_eq!(authorization(&request), "Bearer a-key");
    assert_eq!(body["model"], "vendor-a");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(&messages[..5], completion_history.as_slice());
    assert_eq!(messages.len(), 12);
    slash_assert_message(&messages[5], "assistant", "completion final");
    slash_assert_message(&messages[6], "user", "response second task");
    slash_assert_merged_response_batch(&messages[7]);
    for (index, old, id) in [
        (8, &response_results[0], "response-write"),
        (9, &response_results[1], "response-read"),
    ] {
        assert_eq!(messages[index]["content"], old["output"]);
        assert_eq!(result(&messages[index], id), response_result(old, id));
    }
    slash_assert_message(&messages[10], "assistant", "response final");
    slash_assert_message(&messages[11], "user", "completion third task");
    assert!(!body.to_string().contains("opaque-switch-marker"));
    assert!(!body.to_string().contains("reasoning-private-id"));
    let third_history = messages.clone();
    let calls = json!([
        call(
            "third-write",
            "write",
            json!({"path":"third.txt","content":"gamma"})
        ),
        call("third-read", "read", json!({"path":"third.txt"}))
    ]);
    calls_reply(request, calls.clone());
    let (request, body) = receive(&a);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(&messages[..12], third_history.as_slice());
    assert_eq!(messages[12]["tool_calls"], calls);
    assert_eq!(result(&messages[13], "third-write")["bytes_written"], 5);
    assert_eq!(result(&messages[14], "third-read")["ok"], true);
    final_reply(request, "third final");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        b"completion commentary\ncompletion final\nresponse commentarybeforeafter\nresponse final\nthird final\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("completion.txt")).unwrap(),
        "completion sentinel"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("response.txt")).unwrap(),
        "response sentinel"
    );
    no_more(&a);
    no_more(&b);
}

#[test]
fn model_switch_failure_is_atomic_and_catalog_reloads_without_clearing_history() {
    let a = server();
    let b = server();
    let bad = server();
    let dir = tempfile::tempdir().unwrap();
    let mut invalid_header = slash_provider(
        &bad,
        "bad",
        "response",
        json!({"broken":{"model":"vendor-broken"}}),
    );
    invalid_header["api_key"] = json!("switch-secret-marker\ninvalid-header");
    let models = json!({"providers": {
        "a": slash_provider(&a, "a", "completion", json!({"primary": {"model":"vendor-a"}})),
        "gateway": slash_provider(&b, "gateway", "completion", json!({
            "team/secondary": {"model":"vendor-b"}})),
        "bad": invalid_header
    }})
    .to_string();
    let settings = "{}";
    let mut cli = Cli::start_with(dir.path(), None, settings, &models);
    cli.input("establish history\n");
    let (request, body) = receive(&a);
    assert_eq!(authorization(&request), "Bearer a-key");
    let system = body["messages"][0].clone();
    // The first request proves startup loading finished before files change.
    assert_eq!(
        std::fs::read(config_path(dir.path(), "models.yml")).unwrap(),
        models.as_bytes()
    );
    assert_eq!(
        std::fs::read(config_path(dir.path(), "settings.yml")).unwrap(),
        settings.as_bytes()
    );
    let replacement_settings = r#"{"notification_timeout_seconds":9}"#;
    let mut replacement_catalog: Value = serde_json::from_str(&models).unwrap();
    replacement_catalog["providers"]["replacement"] = slash_provider(
        &bad,
        "replacement",
        "response",
        json!({"new": {"model":"newly-discovered-model"}}),
    );
    let replacement_models = replacement_catalog.to_string();
    std::fs::write(
        config_path(dir.path(), "settings.yml"),
        replacement_settings,
    )
    .unwrap();
    std::fs::write(config_path(dir.path(), "models.yml"), &replacement_models).unwrap();
    let calls = json!([
        call(
            "atomic-write",
            "write",
            json!({"path":"atomic.txt","content":"remembered"})
        ),
        call("atomic-read", "read", json!({"path":"atomic.txt"}))
    ]);
    calls_reply(request, calls.clone());
    let (request, body) = receive(&a);
    assert_eq!(body["messages"][2]["tool_calls"], calls);
    assert_eq!(
        result(&body["messages"][3], "atomic-write")["bytes_written"],
        10
    );
    assert_eq!(result(&body["messages"][4], "atomic-read")["ok"], true);
    let history = body["messages"].as_array().unwrap().clone();
    final_reply(request, "history established");
    std::fs::write(dir.path().join("atomic.txt"), "atomic sentinel").unwrap();

    cli.input("/model bad/broken\n/model missing/nope\nstill use a\n");
    let (request, body) = receive(&a);
    assert_eq!(authorization(&request), "Bearer a-key");
    assert_eq!(body["model"], "vendor-a");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 7);
    assert_eq!(&messages[..5], history.as_slice());
    slash_assert_message(&messages[5], "assistant", "history established");
    slash_assert_message(&messages[6], "user", "still use a");
    let before_success = messages.clone();
    final_reply(request, "old model intact");

    cli.input("/model\n/model gateway/team/secondary\nuse startup b\n");
    let (request, body) = receive(&b);
    assert_eq!(authorization(&request), "Bearer gateway-key");
    assert_eq!(body["model"], "vendor-b");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 9);
    assert_eq!(messages[0], system);
    assert_eq!(&messages[..7], before_success.as_slice());
    slash_assert_message(&messages[7], "assistant", "old model intact");
    slash_assert_message(&messages[8], "user", "use startup b");
    final_reply(request, "startup b intact");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        b"history established\nold model intact\nstartup b intact\n"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("model not found; use /model to list configured models"),
        "{stderr}"
    );
    assert!(stderr.to_lowercase().contains("header"), "{stderr}");
    assert!(
        stderr
            .lines()
            .any(|line| line.contains('*') && line.contains("a/primary"))
    );
    assert!(stderr.contains("gateway/team/secondary"), "{stderr}");
    assert!(stderr.contains("vendor-b"), "{stderr}");
    assert!(stderr.contains("replacement/new"), "{stderr}");
    assert!(stderr.contains("newly-discovered-model"), "{stderr}");
    for secret in [
        "switch-secret-marker",
        "invalid-header",
        "a-key",
        "gateway-key",
    ] {
        assert!(!stderr.contains(secret), "leaked {secret}: {stderr}");
    }
    assert_eq!(
        std::fs::read(config_path(dir.path(), "models.yml")).unwrap(),
        replacement_models.as_bytes()
    );
    assert_eq!(
        std::fs::read(config_path(dir.path(), "settings.yml")).unwrap(),
        replacement_settings.as_bytes()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("atomic.txt")).unwrap(),
        "atomic sentinel"
    );
    no_more(&a);
    no_more(&b);
    no_more(&bad);
}
