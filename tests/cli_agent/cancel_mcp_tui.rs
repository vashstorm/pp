fn cancel_mcp_settings(mode: &str, block_restart: bool) -> String {
    let mut settings = super::mcp::settings(mode);
    settings["mcpServers"]["local"]["timeout"] = json!(120_000);
    if block_restart {
        settings["mcpServers"]["local"]["env"]["MCP_RESTART_INIT_WAIT"] = json!("1");
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

// 反向断言：进入 Plan 不再断开 MCP 服务，进程必须仍在运行。信号 0 探测不递送信号。
fn cancel_mcp_assert_pid_alive(path: &Path) {
    let pid: i32 = std::fs::read_to_string(path).unwrap().trim().parse().unwrap();
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    assert!(
        rustix::process::test_kill_process(pid).is_ok(),
        "MCP process {pid} is not running"
    );
}

fn cancel_mcp_protocol_path(protocol: &str) -> &'static str {
    if protocol == "completion" { "/v1/chat/completions" } else { "/v1/responses" }
}

fn cancel_mcp_reply_calls(
    request: CapturedRequest,
    protocol: &str,
    entries: &[(&str, &str, Value)],
) {
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

fn cancel_mcp_begin(pty: &mut Pty, provider: &TestServer, dir: &Path, protocol: &str) {
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
    pty.wait_for("取消完成状态行", |screen| {
        cancel_count_normalized(screen, CANCEL_DONE) == 1
    });
    pty.ready();
    cancel_mcp_assert_pid_stopped(&dir.join("mcp-log.cancel-server.pid"));
    cancel_mcp_assert_pid_stopped(&dir.join("mcp-log.cancel-child.pid"));
    assert!(!dir.join("skipped-mcp.txt").exists());
    no_more(provider);
    assert_eq!(cancel_mcp_log(dir).iter().filter(|row| row["method"] == "initialize").count(), 1);
}

#[test]
fn tui_cancel_mcp_stops_real_work_and_restarts_only_for_new_call() {
    let protocol = "response";
        let provider = server();
        let dir = tempfile::tempdir().unwrap();
        crate::write_settings(dir.path(), cancel_mcp_settings("cancel_once", false)).unwrap();
        std::fs::write(config_path(dir.path(), "models.yml"), response_models(&provider, Some(protocol))).unwrap();
        let mut pty = Pty::start_with_args(dir.path(), dir.path(), &["--yolo"]);
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

