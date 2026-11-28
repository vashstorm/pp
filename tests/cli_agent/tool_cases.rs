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
    assert_eq!(bash["output"].as_str().unwrap().trim(), "1");
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
fn eval_persists_state_and_bridges_tools_end_to_end() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Cli::start(&server, dir.path(), None);

    cli.input("compute with python\n");
    let (request, body) = receive(&server);
    let has_eval = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["function"]["name"] == "eval");
    if !has_eval {
        final_reply(request, "no python");
        cli.input("/quit\n");
        let _ = cli.finish();
        return;
    }
    calls_reply(
        request,
        json!([call(
            "e1",
            "eval",
            json!({
                "language": "py",
                "title": "setup and write",
                "code": "secret_val = 21\nwrite('py_out.txt', 'from-py\\n')\nprint('step1 ok')"
            })
        )]),
    );
    let (request, body) = receive(&server);
    let r1 = result(&body["messages"][3], "e1");
    assert_eq!(r1["ok"], true, "{r1}");
    assert_eq!(r1["output"], "step1 ok\n");
    final_reply(request, "step 1 done");

    cli.input("reuse state and read\n");
    let (request, _) = receive(&server);
    calls_reply(
        request,
        json!([call(
            "e2",
            "eval",
            json!({
                "language": "py",
                "code": "body = read('py_out.txt').strip()\nprint(f'{body}:{secret_val * 2}')"
            })
        )]),
    );
    let (request, body) = receive(&server);
    let messages = body["messages"].as_array().unwrap();
    let r2 = result(messages.last().unwrap(), "e2");
    assert_eq!(r2["ok"], true, "{r2}");
    assert_eq!(r2["output"], "from-py:42\n");
    final_reply(request, "all done");

    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "step 1 done\nall done\n"
    );
    no_more(&server);
}
