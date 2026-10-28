use super::*;

#[test]
fn dependent_batch_and_cross_turn_memory() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Cli::start(&server, dir.path(), None);
    cli.input("create and verify a file\n");
    let (request, _) = receive(&server);
    let calls = json!([
        call(
            "w",
            "write",
            json!({"path":"demo.txt","content":"hello from rust agent\n"})
        ),
        call("r", "read", json!({"path":"demo.txt"})),
        call("b", "bash", json!({"command":"wc -l < demo.txt"}))
    ]);
    calls_reply(request, calls.clone());
    let (request, body) = receive(&server);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["tool_calls"], calls);
    assert_eq!(result(&messages[3], "w")["bytes_written"], 22);
    let read = result(&messages[4], "r");
    assert_eq!(read["ok"], true);
    let bash = result(&messages[5], "b");
    assert_eq!(bash["ok"], true);
    assert_eq!(bash["exit_code"], 0);
    assert_eq!(bash["stdout"].as_str().unwrap().trim(), "1");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("demo.txt")).unwrap(),
        "hello from rust agent\n"
    );
    final_reply(request, "verified: hello from rust agent; lines=1");
    cli.input("which file did you just create?\n");
    let (request, body) = receive(&server);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(
        messages[6]["content"],
        "verified: hello from rust agent; lines=1"
    );
    assert_eq!(messages[7]["role"], "user");
    assert_eq!(messages[7]["content"], "which file did you just create?");
    final_reply(request, "demo.txt");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "verified: hello from rust agent; lines=1\ndemo.txt\n"
    );
    no_more(&server);
}

#[test]
fn tool_errors_allow_correction_and_pair_all_ids() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let cli = Cli::start(&server, dir.path(), Some("fix the failed operations"));
    let (request, _) = receive(&server);
    calls_reply(
        request,
        json!([
            call("unknown", "missing", json!({})),
            call("read", "read", json!({"path":"absent.txt"}))
        ]),
    );
    let (request, body) = receive(&server);
    for (index, id) in [(3, "unknown"), (4, "read")] {
        let error = result(&body["messages"][index], id);
        assert_eq!(error["ok"], false);
        assert!(
            error["error"]
                .as_str()
                .unwrap()
                .contains(if id == "unknown" {
                    "unknown tool"
                } else {
                    "absent.txt"
                })
        );
    }
    calls_reply(
        request,
        json!([call(
            "fix",
            "write",
            json!({"path":"absent.txt","content":"recovered"})
        )]),
    );
    let (request, body) = receive(&server);
    assert_eq!(result(&body["messages"][6], "fix")["ok"], true);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("absent.txt")).unwrap(),
        "recovered"
    );
    final_reply(request, "recovered");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"recovered\n");
}

#[test]
fn invalid_batch_never_executes_even_its_first_valid_write() {
    for duplicate in [false, true] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::start(&server, dir.path(), Some("do not write a broken response"));
        let (request, _) = receive(&server);
        let first = call(
            "same",
            "write",
            json!({"path":"must-not-exist","content":"bad"}),
        );
        let mut second = call("same", "read", json!({"path":"must-not-exist"}));
        if !duplicate {
            second.as_object_mut().unwrap().remove("id");
        }
        calls_reply(request, json!([first, second]));
        let output = cli.finish();
        assert!(!output.status.success());
        if duplicate {
            assert!(String::from_utf8_lossy(&output.stderr).contains("duplicate"));
        }
        assert!(!dir.path().join("must-not-exist").exists());
        no_more(&server);
    }
}

#[test]
fn repl_recovers_from_protocol_error_and_new_discards_history() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Cli::start(&server, dir.path(), None);
    cli.input("first\n");
    let (request, _) = receive(&server);
    reply(request, json!({"role":"assistant","content":null}));
    cli.input("second\n");
    let (request, body) = receive(&server);
    assert_eq!(body["messages"][1]["content"], "first");
    assert_eq!(body["messages"][2]["content"], "second");
    final_reply(request, "second answer");
    cli.input("/new\nthird\n");
    let (request, body) = receive(&server);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["content"], "third");
    final_reply(request, "fresh answer");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"second answer\nfresh answer\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("neither text nor tool calls"));
}

#[test]
fn provider_model_selection_routes_to_second_provider_second_model() {
    let unused = server();
    let selected = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({
        "providers": {
            "openai": {
                "protocol": "completion",
                "api_key": "first-key",
                "base_url": format!("http://{}/v1", unused.server_addr()),
                "models": {
                    "primary": { "model": "first-primary"},
                    "secondary": { "model": "first-secondary"}}
            },
            "gateway": {
                "protocol": "completion",
                "api_key": "second-key",
                "base_url": format!("http://{}/v1", selected.server_addr()),
                "models": {
                    "primary": { "model": "second-primary"},
                    "team/secondary": { "model": "second-secondary"}}
            }}
    })
    .to_string();
    let mut cli = Cli::start_with(
        dir.path(),
        None,
        "{\"default_model\": \"gateway/team/secondary\"}",
        &models,
    );
    cli.input("create the proof file\n");
    let (request, body) = receive(&selected);
    assert_eq!(body["model"], "second-secondary");
    assert_eq!(authorization(&request), "Bearer second-key");
    calls_reply(
        request,
        json!([call(
            "proof",
            "write",
            json!({"path":"proof.txt","content":"from second provider\n"})
        )]),
    );
    let (request, body) = receive(&selected);
    assert_eq!(body["model"], "second-secondary");
    assert_eq!(authorization(&request), "Bearer second-key");
    assert_eq!(result(&body["messages"][3], "proof")["ok"], true);
    final_reply(request, "done");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"done\n");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("proof.txt")).unwrap(),
        "from second provider\n"
    );
    no_more(&unused);
    no_more(&selected);
}

#[test]
fn streaming_broken_stdout_still_records_executed_tool_results() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Cli::start(&server, dir.path(), Some("write with closed display"));
    drop(cli.0.as_mut().unwrap().stdout.take());
    let (request, _) = receive(&server);
    reply(
        request,
        json!({
            "role":"assistant", "content":"visible attempt",
            "tool_calls":[call("write", "write", json!({"path":"saved","content":"record me"}))]
        }),
    );
    let (request, body) = receive(&server);
    assert_eq!(result(&body["messages"][3], "write")["bytes_written"], 9);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("saved")).unwrap(),
        "record me"
    );
    final_reply(request, "finished history");
    let output = cli.finish();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("writing task output"));
    no_more(&server);
}
