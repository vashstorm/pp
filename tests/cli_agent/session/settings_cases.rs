use super::*;

#[test]
fn session_settings_persist_across_model_switch_and_restore() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    let models = json!({"providers":{
        "a": slash_provider(&server, "a", "completion", json!({
            "primary": {"model":"a-one"},"secondary": {"model":"a-two"}})),
        "b": slash_provider(&server, "b", "response", json!({"primary": {"model":"b-one"}}))}})
    .to_string();
    configure(home.path(), r#"{"default_model":"a/primary"}"#, &models);

    let mut cli = start(home.path(), project.path(), &[]);
    cli.input("first\n");
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "a-one");
    Api::Chat.assert_level(&body, "none");
    final_reply(request, "first answer");
    cli.input("/effort max\n/model a/secondary\n");
    cli.wait_for(Pipe::Err, "Current model: a/secondary (a-two)");
    cli.input("second\n");
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "a-two");
    Api::Chat.assert_level(&body, "max");
    let (users, assistants) = Api::Chat.turns(&body);
    assert_eq!(users, ["first", "second"]);
    assert_eq!(assistants, ["first answer"]);
    final_reply(request, "second answer");
    cli.input("/quit\n");
    let first_output = cli.finish();
    assert!(
        first_output.status.success(),
        "{}",
        String::from_utf8_lossy(&first_output.stderr)
    );
    let id = only_session_id(home.path());
    let path = sessions_dir(home.path()).join(format!("{id}.jsonl"));
    let same_protocol = records(&path);
    assert_session_contract(&same_protocol, &id, project.path());
    let saved_request = rebuild_request(&same_protocol);
    assert_eq!(saved_request["model"], "a-two");
    Api::Chat.assert_level(&saved_request, "max");
    let same_protocol_messages = canonical_messages(&same_protocol);
    assert_eq!(same_protocol_messages.len(), 4);
    let mut cli = start(home.path(), launch.path(), &["-r", &id]);
    cli.wait_for(Pipe::Err, &format!("Resumed session: {id}"));
    no_more(&server);
    assert_eq!(records(&path), same_protocol);
    cli.input("/model b/primary\n");
    cli.wait_for(Pipe::Err, "Current model: b/primary (b-one)");
    cli.input("third\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    assert_eq!(body["model"], "b-one");
    Api::Responses.assert_level(&body, "max");
    let (users, assistants) = Api::Responses.turns(&body);
    assert_eq!(users, ["first", "second", "third"]);
    assert_eq!(assistants, ["first answer", "second answer"]);
    assert!(
        body["instructions"]
            .as_str()
            .unwrap()
            .contains("coding assistant")
    );
    response_final(request, "third answer");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        [first_output.stdout, output.stdout].concat(),
        b"first answer\nsecond answer\nthird answer\n"
    );

    let saved = records(&path);
    assert_session_contract(&saved, &id, project.path());
    assert_eq!(&saved[..same_protocol.len()], same_protocol.as_slice());
    let messages = canonical_messages(&saved);
    assert_eq!(messages.len(), 6);
    assert_eq!(
        &messages[..same_protocol_messages.len()],
        same_protocol_messages.as_slice()
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["role"] == "user")
            .map(|message| message["content"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["first", "second", "third"]
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["role"] == "assistant")
            .map(|message| message["model"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["a-one", "a-two", "b-one"]
    );
    assert_eq!(
        saved
            .iter()
            .filter(|record| record["type"] == "model_change")
            .map(|record| record["model"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["a/primary", "a/secondary", "b/primary"]
    );
    assert!(saved.iter().any(
        |record| record["type"] == "thinking_level_change" && record["thinkingLevel"] == "max"
    ));
    assert_eq!(
        saved
            .iter()
            .filter(|record| record["customType"] == "pp.session_state")
            .count(),
        3
    );
    let rebuilt = rebuild_request(&saved);

    // After restart the next request carries the saved model, think level
    // and converted history (replayed from the JSONL, including the
    // cross-protocol checkpoint).
    let cli = start(
        home.path(),
        launch.path(),
        &["-r", &id, "-p", "after restore"],
    );
    let (request, body) = receive_at(&server, "/v1/responses");
    let mut expected = rebuilt;
    let mut new_user = expected["input"][0].clone();
    match &mut new_user["content"] {
        Value::String(text) => *text = "after restore".to_owned(),
        Value::Array(parts) => parts[0]["text"] = json!("after restore"),
        content => panic!("unexpected user content: {content}"),
    }
    expected["input"].as_array_mut().unwrap().push(new_user);
    assert_eq!(
        body, expected,
        "restored request must replay the saved native history"
    );
    assert_eq!(body["model"], "b-one");
    assert_eq!(body["reasoning"]["effort"], "max");
    response_final(request, "restored answer");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"restored answer\n");
    let restored = records(&path);
    assert_session_contract(&restored, &id, project.path());
    assert_eq!(&restored[..saved.len()], saved.as_slice());
    let restored_messages = canonical_messages(&restored);
    assert_eq!(&restored_messages[..messages.len()], messages.as_slice());
    assert_eq!(restored_messages.len(), 8);
    assert_eq!(restored_messages[6]["role"], "user");
    assert_eq!(restored_messages[6]["content"], "after restore");

    // Switching settings without an active session writes nothing.
    let idle = tempfile::tempdir().unwrap();
    configure(idle.path(), r#"{"default_model":"a/primary"}"#, &models);
    let mut cli = start(idle.path(), project.path(), &[]);
    cli.input("/effort high\n/model b/primary\n/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!sessions_dir(idle.path()).exists());
    no_more(&server);
}

#[test]
fn model_thinking_session_restore_preserves_saved_level_across_config_changes() {
    for api in APIS {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let launch = tempfile::tempdir().unwrap();
        let protocol = match api {
            Api::Chat => "completion",
            Api::Responses => "response",
        };
        let configured_models = |default_level: &str, efforts: Value| {
            json!({"providers":{
            "p": slash_provider(&server, "p", protocol, json!({
                "plain": {"model":"m-plain"},
                "cfg": {"model":"m-cfg","thinking":{
                    "efforts":efforts,"defaultLevel":default_level
                }}}))}})
            .to_string()
        };
        // 首次启动仍由旧四字段经迁移导入 agent.db；迁移完成后偏好由数据库权威，
        // 重启只改写 settings.yml 中的其余字段。
        let seed_settings = r#"{"default_model":"p/plain","think_level":"low"}"#;
        let live_settings = "{}";
        configure(
            home.path(),
            seed_settings,
            &configured_models("high", json!(["low", "high"])),
        );

        // Startup on the unconfigured model keeps the global level.
        let mut cli = start(home.path(), project.path(), &[]);
        cli.input("first\n");
        let (request, body) = api.receive(&server);
        assert_eq!(body["model"], "m-plain");
        api.assert_level(&body, "low");
        api.final_reply(request, "first answer");

        // An actual switch applies the target model's explicit default even
        // though the current level is also supported there.
        cli.input("/model p/cfg\n");
        cli.wait_for(Pipe::Err, "Current model: p/cfg (m-cfg)");
        let id = only_session_id(home.path());
        let path = sessions_dir(home.path()).join(format!("{id}.jsonl"));
        let switched_request = rebuild_request(&records(&path));
        assert_eq!(switched_request["model"], "m-cfg");
        api.assert_level(&switched_request, "high");
        let before_rejected = fs::read(&path).unwrap();
        cli.input("/effort max\n");
        cli.wait_for(
            Pipe::Err,
            "error: thinking level is not supported by the current model; options: low, high",
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            before_rejected,
            "rejected effort command must not touch the journal"
        );
        cli.input("second\n");
        let (request, body) = api.receive(&server);
        assert_eq!(body["model"], "m-cfg");
        api.assert_level(&body, "high");
        let (users, assistants) = api.turns(&body);
        assert_eq!(users, ["first", "second"]);
        assert_eq!(assistants, ["first answer"]);
        api.final_reply(request, "second answer");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        // The saved native state rebuilds to exactly the model and effort of
        // the real request.
        let saved = records(&path);
        assert_session_contract(&saved, &id, project.path());
        let saved_request = rebuild_request(&saved);
        assert_eq!(saved_request["model"], body["model"]);
        match api {
            Api::Chat => assert_eq!(saved_request["reasoning_effort"], body["reasoning_effort"]),
            Api::Responses => assert_eq!(saved_request["reasoning"], body["reasoning"]),
        }

        // A later defaultLevel change never rewrites the saved level or the
        // replayed native history.
        configure(
            home.path(),
            live_settings,
            &configured_models("low", json!(["low", "high"])),
        );
        let cli = start(
            home.path(),
            launch.path(),
            &["-r", &id, "-p", "after restore"],
        );
        let (request, body) = api.receive(&server);
        let field = match api {
            Api::Chat => "messages",
            Api::Responses => "input",
        };
        let mut expected = saved_request;
        let mut new_user = expected[field]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["role"] == "user")
            .expect("saved history must contain a user turn")
            .clone();
        match &mut new_user["content"] {
            Value::String(text) => *text = "after restore".to_owned(),
            Value::Array(parts) => parts[0]["text"] = json!("after restore"),
            content => panic!("unexpected user content: {content}"),
        }
        expected[field].as_array_mut().unwrap().push(new_user);
        assert_eq!(
            body, expected,
            "restored request must replay the saved native history and effort"
        );
        assert_eq!(body["model"], "m-cfg");
        api.assert_level(&body, "high");
        api.final_reply(request, "restored answer");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"restored answer\n");
        no_more(&server);

        // Narrowing efforts below the saved level blocks restore before any
        // request or journal write, byte for byte.
        let before_failure = fs::read(&path).unwrap();
        configure(
            home.path(),
            live_settings,
            &configured_models("medium", json!(["medium", "max"])),
        );
        expect_restore_failure(
            &server,
            home.path(),
            launch.path(),
            &["-r", &id, "-p", "blocked"],
            &["saved session thinking level is not supported by the configured model"],
            &["p-key"],
        );
        assert_eq!(fs::read(&path).unwrap(), before_failure);
    }
}

#[test]
fn session_restore_keeps_saved_edit_mode_and_execution_limits() {
    for api in APIS {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let launch = tempfile::tempdir().unwrap();
        fs::write(project.path().join("source.txt"), "alpha\nbeta\ngamma\n").unwrap();
        fs::write(project.path().join("oversized.txt"), "x".repeat(128)).unwrap();
        let models = api.models(&server);
        configure(
            home.path(),
            r#"{"default_model":"fixture/primary","skill_dirs":[],"edit":"hashline","read_max_bytes":64,"read_max_lines":1,"bash_timeout_seconds":1}"#,
            &models,
        );
        let cli = start(home.path(), project.path(), &["-p", "save tool policy"]);
        let (request, _) = api.receive(&server);
        api.final_reply(request, "policy saved");
        assert!(cli.finish().status.success());
        let id = only_session_id(home.path());
        let path = sessions_dir(home.path()).join(format!("{id}.jsonl"));
        let saved = rebuild_request(&records(&path));

        // Current settings must not silently replace the saved tool definitions or execution policy.
        // 默认模型已由 agent.db 权威，这里只改写仍属于 settings.yml 的工具策略。
        configure(
            home.path(),
            r#"{"skill_dirs":[],"edit":"replace","read_max_bytes":1024,"read_max_lines":10,"bash_timeout_seconds":30}"#,
            &models,
        );
        let cli = start(home.path(), launch.path(), &["-c", &id]);
        let (request, body) = api.receive(&server);
        assert_eq!(body, saved);
        api.calls_reply(
            request,
            json!([
                api.call("read-policy", "read", json!({"path":"source.txt"})),
                api.call("byte-policy", "read", json!({"path":"oversized.txt"})),
                api.call(
                    "timeout-policy",
                    "bash",
                    json!({"command":"sleep 3; printf should-not-appear"})
                )
            ]),
        );
        let (request, body) = api.receive(&server);
        let read = api.tool_result(&body, "read-policy");
        assert_eq!(read["ok"], true);
        assert_eq!(read["lines_read"], 1);
        assert_eq!(read["next_offset"], 2);
        let content = read["content"].as_str().unwrap();
        assert!(content.starts_with("[source.txt#"));
        assert_eq!(content.lines().nth(1), Some("1:alpha"));
        assert!(!content.contains("beta"));
        assert_eq!(api.tool_result(&body, "byte-policy")["ok"], false);
        let bash = api.tool_result(&body, "timeout-policy");
        assert_eq!(bash["timed_out"], true);
        assert_eq!(bash["ok"], false);
        assert!(
            !bash["stdout"]
                .as_str()
                .unwrap()
                .contains("should-not-appear")
        );
        api.final_reply(request, "saved policy enforced");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"saved policy enforced\n");
        let records = records(&path);
        assert_session_contract(&records, &id, project.path());
        let messages = canonical_messages(&records);
        let tools: Vec<_> = messages
            .iter()
            .filter(|message| message["role"] == "toolResult")
            .collect();
        assert_eq!(tools.len(), 3);
        for (index, (call_id, tool_name, is_error)) in [
            ("read-policy", "read", false),
            ("byte-policy", "read", true),
            ("timeout-policy", "bash", true),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(tools[index]["toolCallId"], call_id);
            assert_eq!(tools[index]["toolName"], tool_name);
            assert_eq!(tools[index]["isError"], is_error);
            let raw: Value =
                serde_json::from_str(tools[index]["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(tools[index]["details"], raw);
        }
        no_more(&server);
    }
}

#[test]
fn session_first_create_failure_is_deterministic_without_invented_session() {
    for api in APIS {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        configure(home.path(), fixture_settings(), &api.models(&server));
        // A regular file where the sessions directory belongs makes the
        // first create fail deterministically regardless of privileges.
        fs::write(sessions_dir(home.path()), "not a directory").unwrap();
        let cli = start(home.path(), cwd.path(), &["-p", "hello"]);
        let (request, _) = api.receive(&server);
        // Headers are enough to trigger the create-on-open hook; the task
        // aborts before any stream content is consumed.
        let stream = SseReply::new(request);
        let output = cli.finish();
        drop(stream);
        assert!(
            !output.status.success(),
            "task must fail when the journal cannot be created"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("persisting session"), "{stderr}");
        assert!(!stderr.contains("fixture-key"));
        assert!(output.stdout.is_empty());
        no_more(&server);
        assert_eq!(
            fs::read_to_string(sessions_dir(home.path())).unwrap(),
            "not a directory"
        );
        assert!(fs::metadata(sessions_dir(home.path())).unwrap().is_file());
    }
}

#[test]
fn session_file_size_limit_latches_after_executing_whole_tool_batch() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    configure(home.path(), fixture_settings(), &fixture_models(&server));
    // Ignore SIGXFSZ and cap file sizes: the session journal write of a huge
    // streamed delta fails with EFBIG after the checkpoint and user records
    // were already written, without touching the (small) tool files.
    let mut command = Command::new("bash");
    command
        .arg("-c")
        .arg("trap '' XFSZ; ulimit -f 256; exec \"$@\"")
        .arg("pp")
        .arg(env!("CARGO_BIN_EXE_pp"))
        .args(["-p", "overflow the journal"])
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let cli = Live::new(Cli(Some(command.spawn().unwrap())));
    let (request, _) = receive(&server);
    let big = "A".repeat(400_000);
    reply(
        request,
        json!({
            "role":"assistant","content":big,
            "tool_calls":[
                call("one", "write", json!({"path":"first.txt","content":"one"})),
                call("two", "write", json!({"path":"second.txt","content":"two"}))
            ]
        }),
    );
    let output = cli.finish();
    assert!(
        !output.status.success(),
        "task must fail once persistence latched an error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("persisting session"), "{stderr}");
    // The display kept streaming while persistence had already failed.
    assert!(output.stdout.len() > 300_000);
    // The accepted whole tool batch executed despite the latch.
    assert_eq!(
        fs::read_to_string(cwd.path().join("first.txt")).unwrap(),
        "one"
    );
    assert_eq!(
        fs::read_to_string(cwd.path().join("second.txt")).unwrap(),
        "two"
    );
    // No next HTTP request after the failed batch commit.
    no_more(&server);
    let id = only_session_id(home.path());
    let bytes = fs::read(sessions_dir(home.path()).join(format!("{id}.jsonl"))).unwrap();
    // The latched write left an unterminated partial record.
    assert!(!bytes.ends_with(b"\n"));
    assert!(String::from_utf8_lossy(&bytes).contains("\"type\":\"session\""));
    assert!(String::from_utf8_lossy(&bytes).contains("\"customType\":\"pp.session_state\""));
}
