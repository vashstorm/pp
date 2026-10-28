use super::*;

#[test]
fn model_switch_completion_alias_routing_preserves_tools_and_reset_selection() {
    let a = server();
    let b = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers": {
        "a": slash_provider(&a, "a", "completion", json!({
            "primary": { "model": "vendor-a"}})),
        "gateway": slash_provider(&b, "gateway", "completion", json!({
            "team/secondary": { "model": "vendor-b"}}))}})
    .to_string();
    let mut cli = Cli::start_with(
        dir.path(),
        None,
        r#"{"default_model":"a/primary"}"#,
        &models,
    );
    cli.input("write then read alpha\n");
    let (request, body) = receive(&a);
    assert_eq!(authorization(&request), "Bearer a-key");
    assert_eq!(body["model"], "vendor-a");
    let system = body["messages"][0].clone();
    let calls = json!([
        call(
            "write-alpha",
            "write",
            json!({"path":"alpha.txt","content":"alpha"})
        ),
        call("read-alpha", "read", json!({"path":"alpha.txt"}))
    ]);
    calls_reply(request, calls.clone());
    let (request, body) = receive(&a);
    assert_eq!(body["messages"][2]["tool_calls"], calls);
    assert_eq!(
        result(&body["messages"][3], "write-alpha")["bytes_written"],
        5
    );
    assert_eq!(result(&body["messages"][4], "read-alpha")["ok"], true);
    let history = body["messages"].as_array().unwrap().clone();
    final_reply(request, "alpha saved");

    // A sentinel file makes replaying an old write observable.
    std::fs::write(dir.path().join("alpha.txt"), "do not replay").unwrap();
    cli.input("/model gateway/team/secondary\nrecall alpha\n");
    let (request, body) = receive(&b);
    assert_eq!(authorization(&request), "Bearer gateway-key");
    assert_eq!(body["model"], "vendor-b");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(&messages[..history.len()], history.as_slice());
    slash_assert_message(&messages[5], "assistant", "alpha saved");
    slash_assert_message(&messages[6], "user", "recall alpha");
    assert_eq!(messages.len(), 7);
    final_reply(request, "alpha recalled");

    cli.input("/model gateway/team/secondary\nrecall again\n");
    let (request, body) = receive(&b);
    assert_eq!(body["model"], "vendor-b");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(&messages[..history.len()], history.as_slice());
    slash_assert_message(&messages[7], "assistant", "alpha recalled");
    slash_assert_message(&messages[8], "user", "recall again");
    assert_eq!(messages.len(), 9);
    final_reply(request, "alpha again");

    cli.input("/new\nfresh task\n");
    let (request, body) = receive(&b);
    assert_eq!(authorization(&request), "Bearer gateway-key");
    assert_eq!(body["model"], "vendor-b");
    assert_eq!(body["messages"][0], system);
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    slash_assert_message(&body["messages"][1], "user", "fresh task");
    final_reply(request, "fresh on b");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        b"alpha saved\nalpha recalled\nalpha again\nfresh on b\n"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr
            .matches("Current model: gateway/team/secondary (vendor-b)")
            .count(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("alpha.txt")).unwrap(),
        "do not replay"
    );
    no_more(&a);
    no_more(&b);
}

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
fn model_switch_responses_first_round_trip_keeps_text_reasoning_and_tools() {
    let a = server();
    let b = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers": {
        "a": slash_provider(&a, "a", "response", json!({"primary": {"model":"vendor-a"}})),
        "b": slash_provider(&b, "b", "completion", json!({"secondary": {"model":"vendor-b"}}))}})
    .to_string();
    let mut cli = Cli::start_with(
        dir.path(),
        None,
        r#"{"default_model":"a/primary"}"#,
        &models,
    );
    cli.input("responses first task\n");
    let (request, body) = receive_at(&a, "/v1/responses");
    let instructions = body["instructions"].clone();
    let batch = slash_response_batch();
    response_reply(request, batch.clone());
    let (request, body) = receive_at(&a, "/v1/responses");
    let input = body["input"].as_array().unwrap();
    assert_eq!(&input[1..6], batch.as_array().unwrap().as_slice());
    assert_eq!(
        response_result(&input[6], "response-write")["bytes_written"],
        4
    );
    assert_eq!(response_result(&input[7], "response-read")["ok"], true);
    let outputs = [input[6].clone(), input[7].clone()];
    response_final(request, "responses first final");
    std::fs::write(dir.path().join("response.txt"), "response sentinel").unwrap();

    cli.input("/model b/secondary\ncompletion second task\n");
    let (request, body) = receive(&b);
    assert_eq!(authorization(&request), "Bearer b-key");
    assert_eq!(body["model"], "vendor-b");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 7);
    slash_assert_message(&messages[0], "system", instructions.as_str().unwrap());
    slash_assert_message(&messages[1], "user", "responses first task");
    slash_assert_merged_response_batch(&messages[2]);
    for (index, old, id) in [
        (3, &outputs[0], "response-write"),
        (4, &outputs[1], "response-read"),
    ] {
        assert_eq!(messages[index]["content"], old["output"]);
        assert_eq!(result(&messages[index], id), response_result(old, id));
    }
    slash_assert_message(&messages[5], "assistant", "responses first final");
    slash_assert_message(&messages[6], "user", "completion second task");
    assert!(!body.to_string().contains("opaque-switch-marker"));
    let calls = json!([
        call(
            "completion-write",
            "write",
            json!({"path":"completion.txt","content":"alpha"})
        ),
        call("completion-read", "read", json!({"path":"completion.txt"}))
    ]);
    reply(
        request,
        json!({"role":"assistant","content":"completion commentary","tool_calls":calls}),
    );
    let (request, body) = receive(&b);
    assert_eq!(body["messages"][7]["tool_calls"], calls);
    assert_eq!(
        result(&body["messages"][8], "completion-write")["bytes_written"],
        5
    );
    assert_eq!(result(&body["messages"][9], "completion-read")["ok"], true);
    let completion_outputs = [body["messages"][8].clone(), body["messages"][9].clone()];
    final_reply(request, "completion second final");
    std::fs::write(dir.path().join("completion.txt"), "completion sentinel").unwrap();

    cli.input("/model a/primary\nresponses third task\n");
    let (request, body) = receive_at(&a, "/v1/responses");
    assert_eq!(authorization(&request), "Bearer a-key");
    assert_eq!(body["model"], "vendor-a");
    assert_eq!(body["instructions"], instructions);
    let input = body["input"].as_array().unwrap();
    slash_assert_portable(input);
    assert_eq!(input.len(), 15);
    response_user(&input[0], "responses first task");
    slash_assert_message(
        &input[1],
        "assistant",
        "response commentary\nsummary one\nsummary two\nreasoning plain\nbeforerefusedafter",
    );
    let original_calls = json!([
        call(
            "response-write",
            "write",
            json!({"path":"response.txt","content":"beta"})
        ),
        call("response-read", "read", json!({"path":"response.txt"}))
    ]);
    slash_assert_response_calls(&input[2..4], &original_calls);
    assert_eq!(input[4]["output"], outputs[0]["output"]);
    assert_eq!(input[5]["output"], outputs[1]["output"]);
    assert_eq!(response_result(&input[4], "response-write")["ok"], true);
    assert_eq!(response_result(&input[5], "response-read")["ok"], true);
    slash_assert_message(&input[6], "assistant", "responses first final");
    response_user(&input[7], "completion second task");
    slash_assert_message(&input[8], "assistant", "completion commentary");
    slash_assert_response_calls(&input[9..11], &calls);
    for (index, old, id) in [
        (11, &completion_outputs[0], "completion-write"),
        (12, &completion_outputs[1], "completion-read"),
    ] {
        assert_eq!(input[index]["output"], old["content"]);
        assert_eq!(response_result(&input[index], id), result(old, id));
    }
    slash_assert_message(&input[13], "assistant", "completion second final");
    response_user(&input[14], "responses third task");
    assert!(!body.to_string().contains("opaque-switch-marker"));
    let third_initial = input.clone();
    let calls = json!([
        response_call(
            "third-write",
            "write",
            json!({"path":"third.txt","content":"gamma"})
        ),
        response_call("third-read", "read", json!({"path":"third.txt"}))
    ]);
    response_reply(request, calls.clone());
    let (request, body) = receive_at(&a, "/v1/responses");
    let input = body["input"].as_array().unwrap();
    assert_eq!(&input[..15], third_initial.as_slice());
    assert_eq!(&input[15..17], calls.as_array().unwrap().as_slice());
    assert_eq!(
        response_result(&input[17], "third-write")["bytes_written"],
        5
    );
    assert_eq!(response_result(&input[18], "third-read")["ok"], true);
    response_final(request, "responses third final");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        b"response commentarybeforeafter\nresponses first final\ncompletion commentary\ncompletion second final\nresponses third final\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("response.txt")).unwrap(),
        "response sentinel"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("completion.txt")).unwrap(),
        "completion sentinel"
    );
    no_more(&a);
    no_more(&b);
}

#[test]
fn model_switch_responses_native_replay_and_cross_provider_portability() {
    let a = server();
    let b = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers": {
        "a": slash_provider(&a, "a", "response", json!({
            "primary": {"model":"vendor-a"},
            "second": {"model":"vendor-a-second"}})),
        "b": slash_provider(&b, "b", "response", json!({"secondary": {"model":"vendor-b"}}))}})
    .to_string();
    let mut cli = Cli::start_with(
        dir.path(),
        None,
        r#"{"default_model":"a/primary"}"#,
        &models,
    );
    cli.input("native response task\n");
    let (request, body) = receive_at(&a, "/v1/responses");
    let instructions = body["instructions"].clone();
    let batch = slash_response_batch();
    response_reply(request, batch.clone());
    let (request, body) = receive_at(&a, "/v1/responses");
    let input = body["input"].as_array().unwrap();
    assert_eq!(&input[1..6], batch.as_array().unwrap().as_slice());
    assert_eq!(
        response_result(&input[6], "response-write")["bytes_written"],
        4
    );
    assert_eq!(response_result(&input[7], "response-read")["ok"], true);
    let native_history = input.clone();
    let final_message =
        response_message("msg-native-final", json!([response_text("native final")]));
    response_reply(request, json!([final_message.clone()]));
    std::fs::write(dir.path().join("response.txt"), "native sentinel").unwrap();

    cli.input("/model a/second\nsame provider task\n");
    let (request, body) = receive_at(&a, "/v1/responses");
    assert_eq!(authorization(&request), "Bearer a-key");
    assert_eq!(body["model"], "vendor-a-second");
    assert_eq!(body["instructions"], instructions);
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 10);
    assert_eq!(&input[..8], native_history.as_slice());
    assert_eq!(input[8], final_message);
    response_user(&input[9], "same provider task");
    // This equality includes provider IDs, status, phase and opaque reasoning.
    assert_eq!(&input[1..6], batch.as_array().unwrap().as_slice());
    assert_eq!(input[3]["encrypted_content"], "opaque-switch-marker");
    response_final(request, "same provider final");

    cli.input("/model b/secondary\ncross provider task\n");
    let (request, body) = receive_at(&b, "/v1/responses");
    assert_eq!(authorization(&request), "Bearer b-key");
    assert_eq!(body["model"], "vendor-b");
    assert_eq!(body["instructions"], instructions);
    assert_eq!(body["store"], false);
    assert!(body.get("previous_response_id").is_none());
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 12);
    slash_assert_portable(input);
    response_user(&input[0], "native response task");
    slash_assert_message(&input[1], "assistant", "response commentary");
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[2]["call_id"], "response-write");
    assert_eq!(input[2]["name"], "write");
    assert_eq!(input[2]["arguments"], batch[1]["arguments"]);
    slash_assert_message(
        &input[3],
        "assistant",
        "summary one\nsummary two\nreasoning plain",
    );
    slash_assert_message(&input[4], "assistant", "beforerefusedafter");
    assert_eq!(input[5]["type"], "function_call");
    assert_eq!(input[5]["call_id"], "response-read");
    assert_eq!(input[5]["name"], "read");
    assert_eq!(input[5]["arguments"], batch[4]["arguments"]);
    for (index, id) in [(6, "response-write"), (7, "response-read")] {
        assert_eq!(input[index]["output"], native_history[index]["output"]);
        assert_eq!(
            response_result(&input[index], id),
            response_result(&native_history[index], id)
        );
    }
    slash_assert_message(&input[8], "assistant", "native final");
    response_user(&input[9], "same provider task");
    slash_assert_message(&input[10], "assistant", "same provider final");
    response_user(&input[11], "cross provider task");
    for marker in [
        "opaque-switch-marker",
        "reasoning-private-id",
        "msg-commentary",
        "msg-mixed",
        "msg-native-final",
        "fc-response-write",
        "fc-response-read",
    ] {
        assert!(
            !body["input"].to_string().contains(marker),
            "replayed {marker}"
        );
    }
    response_final(request, "portable final");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        b"response commentarybeforeafter\nnative final\nsame provider final\nportable final\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("response.txt")).unwrap(),
        "native sentinel"
    );
    no_more(&a);
    no_more(&b);
}

#[test]
fn model_switch_failure_is_atomic_and_catalog_is_a_read_only_startup_snapshot() {
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
    let settings = r#"{"default_model":"a/primary"}"#;
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
    let replacement_settings = r#"{"default_model":"replacement/new"}"#;
    let replacement_models = json!({"providers": {
        "replacement": slash_provider(&bad, "replacement", "response", json!({
            "new": {"model":"not-the-startup-model"}}))}})
    .to_string();
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
    assert!(!stderr.contains("replacement/new"), "{stderr}");
    assert!(!stderr.contains("not-the-startup-model"), "{stderr}");
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
