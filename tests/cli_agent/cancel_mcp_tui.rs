fn cancel_mcp_settings(mode: &str, block_restart: bool) -> String {
    let mut settings = super::mcp::settings(mode);
    settings["mcp_servers"]["local"]["timeout_seconds"] = json!(120);
    if block_restart {
        settings["mcp_servers"]["local"]["env"]["MCP_RESTART_INIT_WAIT"] = json!("1");
    }
    settings.to_string()
}

fn cancel_mcp_wait_file(pty: &mut Pty, path: &Path) {
    pty.wait_for(&format!("MCP marker {}", path.display()), |_| path.is_file());
}

fn cancel_mcp_log(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("mcp-log")).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect()
}

fn cancel_mcp_assert_pid_stopped(path: &Path) {
    let pid: i32 = std::fs::read_to_string(path).unwrap().trim().parse().unwrap();
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    let deadline = Instant::now() + DEADLINE;
    while rustix::process::test_kill_process(pid).is_ok() {
        assert!(Instant::now() < deadline, "MCP process {pid} survived cancellation");
        thread::sleep(Duration::from_millis(10));
    }
}

fn cancel_mcp_protocol_path(protocol: &str) -> &'static str {
    if protocol == "completion" { "/v1/chat/completions" } else { "/v1/responses" }
}

fn cancel_mcp_reply_calls(request: Request, protocol: &str, entries: &[(&str, &str, Value)]) {
    if protocol == "completion" {
        calls_reply(request, entries.iter().map(|(id, name, args)| call(id, name, args.clone())).collect());
    } else {
        response_reply(request, entries.iter().map(|(id, name, args)| response_call(id, name, args.clone())).collect());
    }
}

fn cancel_mcp_outputs(body: &Value, protocol: &str) -> Vec<(String, Value)> {
    let items = body[if protocol == "completion" { "messages" } else { "input" }].as_array().unwrap();
    items.iter().filter_map(|item| {
        if protocol == "completion" && item["role"] == "tool" {
            let id = item["tool_call_id"].as_str().unwrap();
            Some((id.to_owned(), result(item, id)))
        } else if protocol == "response" && item["type"] == "function_call_output" {
            let id = item["call_id"].as_str().unwrap();
            Some((id.to_owned(), response_result(item, id)))
        } else { None }
    }).collect()
}

fn cancel_mcp_begin(pty: &mut Pty, provider: &Server, dir: &Path, protocol: &str) {
    pty.ready();
    pty.input("cancel MCP\r");
    let (request, _) = receive_at(provider, cancel_mcp_protocol_path(protocol));
    cancel_mcp_reply_calls(request, protocol, &[
        ("cancel-old", "mcp__local__echo", json!({"text":"old request"})),
        ("cancel-skip", "write", json!({"path":"skipped-mcp.txt","content":"must not exist"})),
    ]);
    cancel_mcp_wait_file(pty, &dir.join("mcp-log.cancel-ready"));
    cancel_mcp_wait_file(pty, &dir.join("mcp-log.cancel-child.pid"));
    pty.input("\x1b");
    pty.text("Task cancelled. File changes were not rolled back.");
    pty.ready();
    cancel_mcp_assert_pid_stopped(&dir.join("mcp-log.cancel-server.pid"));
    cancel_mcp_assert_pid_stopped(&dir.join("mcp-log.cancel-child.pid"));
    assert!(!dir.join("skipped-mcp.txt").exists());
    no_more(provider);
    assert_eq!(cancel_mcp_log(dir).iter().filter(|row| row["method"] == "initialize").count(), 1);
}

#[test]
fn tui_cancel_mcp_stops_real_work_and_restarts_only_for_new_call() {
    for protocol in ["completion", "response"] {
        let provider = server();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(config_path(dir.path(), "settings.jsonc"), cancel_mcp_settings("cancel_once", false)).unwrap();
        std::fs::write(config_path(dir.path(), "models.jsonc"), response_models(&provider, Some(protocol))).unwrap();
        let mut pty = Pty::start_with_existing_files(dir.path());
        cancel_mcp_begin(&mut pty, &provider, dir.path(), protocol);
        pty.input("new MCP task\r");
        let (request, body) = receive_at(&provider, cancel_mcp_protocol_path(protocol));
        let old = cancel_mcp_outputs(&body, protocol);
        assert_eq!(old.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["cancel-old", "cancel-skip"]);
        assert_eq!(old[0].1["cancelled"], true);
        assert_eq!(old[1].1["cancelled"], true);
        assert_eq!(old[1].1["error"], "Not executed: task cancelled.");
        assert_eq!(cancel_mcp_log(dir.path()).iter().filter(|row| row["method"] == "initialize").count(), 1);
        cancel_mcp_reply_calls(request, protocol, &[("cancel-new", "mcp__local__echo", json!({"text":"new request"}))]);
        let (request, body) = receive_at(&provider, cancel_mcp_protocol_path(protocol));
        let outputs = cancel_mcp_outputs(&body, protocol);
        let output = &outputs.iter().find(|(id, _)| id == "cancel-new").unwrap().1;
        assert_eq!(output["ok"], true);
        assert_eq!(output["result"]["content"][0]["text"], "中文结果");
        let log = cancel_mcp_log(dir.path());
        assert_eq!(log.iter().filter(|row| row["method"] == "initialize").count(), 2);
        let calls: Vec<_> = log.iter().filter(|row| row["method"] == "tools/call").map(|row| row["params"]["arguments"]["text"].as_str().unwrap()).collect();
        assert_eq!(calls, ["old request", "new request"]);
        if protocol == "completion" { final_reply(request, "MCP recovered after cancellation"); } else { response_final(request, "MCP recovered after cancellation"); }
        pty.text("MCP recovered after cancellation");
        pty.input("\x03");
        pty.finish();
        super::mcp::assert_stopped(dir.path());
    }
}

#[test]
fn tui_cancel_mcp_schema_change_rejects_new_call_without_poisoning_local_tools() {
    let provider = server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(config_path(dir.path(), "settings.jsonc"), cancel_mcp_settings("cancel_then_schema_change", false)).unwrap();
    std::fs::write(config_path(dir.path(), "models.jsonc"), fixture_models(&provider)).unwrap();
    let mut pty = Pty::start_with_existing_files(dir.path());
    cancel_mcp_begin(&mut pty, &provider, dir.path(), "completion");
    pty.input("schema check\r");
    let (request, before) = receive(&provider);
    cancel_mcp_reply_calls(request, "completion", &[
        ("schema-rejected", "mcp__local__echo", json!({"text":"must not be sent"})),
        ("local-kept", "write", json!({"path":"local-kept.txt","content":"local still works"})),
    ]);
    let (request, after) = receive(&provider);
    assert_eq!(before["tools"], after["tools"]);
    let outputs = cancel_mcp_outputs(&after, "completion");
    let output = &outputs.iter().find(|(id, _)| id == "schema-rejected").unwrap().1;
    assert_eq!(output["ok"], false);
    assert_eq!(output["error"], "MCP tool definitions changed; restart pp to reconnect.");
    assert_eq!(std::fs::read_to_string(dir.path().join("local-kept.txt")).unwrap(), "local still works");
    assert_eq!(cancel_mcp_log(dir.path()).iter().filter(|row| row["method"] == "tools/call").count(), 1);
    final_reply(request, "local tools survived schema rejection");
    pty.text("local tools survived schema rejection");
    pty.input("try rejected server again\r");
    let (request, _) = receive(&provider);
    calls_reply(request, json!([call("schema-still-rejected", "mcp__local__echo", json!({"text":"not sent either"}))]));
    let (request, body) = receive(&provider);
    assert_eq!(cancel_mcp_outputs(&body, "completion").last().unwrap().1["ok"], false);
    assert_eq!(cancel_mcp_log(dir.path()).iter().filter(|row| row["method"] == "initialize").count(), 2);
    assert_eq!(cancel_mcp_log(dir.path()).iter().filter(|row| row["method"] == "tools/call").count(), 1);
    final_reply(request, "still usable");
    pty.text("still usable");
    pty.input("\x03");
    pty.finish();
    super::mcp::assert_stopped(dir.path());
}

#[test]
fn tui_cancel_mcp_ctrl_c_reaps_replacement_during_rediscovery() {
    let provider = server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(config_path(dir.path(), "settings.jsonc"), cancel_mcp_settings("cancel_once", true)).unwrap();
    std::fs::write(config_path(dir.path(), "models.jsonc"), fixture_models(&provider)).unwrap();
    let mut pty = Pty::start_with_existing_files(dir.path());
    cancel_mcp_begin(&mut pty, &provider, dir.path(), "completion");
    pty.input("restart task\r");
    let (request, _) = receive(&provider);
    calls_reply(request, json!([call("restart-blocked", "mcp__local__echo", json!({"text":"new request"}))]));
    cancel_mcp_wait_file(&mut pty, &dir.path().join("mcp-log.restart-initialize"));
    let new_pid = std::fs::read_to_string(dir.path().join("mcp-log.pid")).unwrap();
    assert_ne!(new_pid, std::fs::read_to_string(dir.path().join("mcp-log.cancel-server.pid")).unwrap());
    pty.input("\x03");
    pty.finish();
    cancel_mcp_assert_pid_stopped(&dir.path().join("mcp-log.pid"));
    assert_eq!(cancel_mcp_log(dir.path()).iter().filter(|row| row["method"] == "tools/call").count(), 1);
    no_more(&provider);
}
