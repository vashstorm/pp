use super::*;

pub(super) fn settings(mode: &str) -> Value {
    json!({"mcpServers":{"local":{
        "command":"bash",
        "args":[format!("{}/tests/fixtures/mcp_server.sh", env!("CARGO_MANIFEST_DIR"))],
        "env":{"MCP_MODE":mode,"MCP_VALUE":"中文结果","MCP_LOG":"mcp-log"},
        "timeout":1000
    }}})
}

fn log(dir: &std::path::Path, name: &str) -> Vec<Value> {
    std::fs::read_to_string(dir.join(name))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn stopped(dir: &std::path::Path, log: &str) {
    let pid: i32 = std::fs::read_to_string(dir.join(format!("{log}.pid")))
        .unwrap()
        .parse()
        .unwrap();
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    assert!(
        rustix::process::test_kill_process(pid).is_err(),
        "{log} MCP 子进程未清理"
    );
}

pub(super) fn assert_stopped(dir: &std::path::Path) {
    stopped(dir, "mcp-log");
}

#[test]
fn mcp_roundtrip_both_protocols_preserves_schema_results_order_and_events() {
    for protocol in ["completion", "response"] {
        let provider = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            dir.path(),
            Some("调用 MCP"),
            &settings("").to_string(),
            &response_models(&provider, Some(protocol)),
        );
        let path = if protocol == "completion" {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        };
        let (request, body) = receive_at(&provider, path);
        let tools = body["tools"].as_array().unwrap();
        let echo = tools
            .iter()
            .find_map(|tool| {
                let tool = if protocol == "completion" {
                    &tool["function"]
                } else {
                    tool
                };
                (tool["name"] == "mcp__local__echo").then_some(tool)
            })
            .unwrap();
        assert_eq!(echo["parameters"]["required"], json!(["text"]));
        assert_eq!(echo["description"], "测试工具");
        assert_eq!(echo["strict"], false);
        let args = json!({"text":"你好\n第二行"});
        if protocol == "completion" {
            calls_reply(
                request,
                json!([
                    call("mcp-1", "mcp__local__echo", args.clone()),
                    call("local-1", "write", json!({"path":"written","content":"ok"})),
                    call("mcp-2", "mcp__local__fail", json!({}))
                ]),
            );
        } else {
            response_reply(
                request,
                json!([
                    response_call("mcp-1", "mcp__local__echo", args.clone()),
                    response_call("local-1", "write", json!({"path":"written","content":"ok"})),
                    response_call("mcp-2", "mcp__local__fail", json!({}))
                ]),
            );
        }
        let (request, body) = receive_at(&provider, path);
        let outputs: Vec<_> = if protocol == "completion" {
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["role"] == "tool")
                .collect()
        } else {
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .collect()
        };
        let results: Vec<Value> = outputs
            .iter()
            .zip(["mcp-1", "local-1", "mcp-2"])
            .map(|(item, id)| {
                if protocol == "completion" {
                    result(item, id)
                } else {
                    response_result(item, id)
                }
            })
            .collect();
        assert_eq!(results[0]["ok"], true);
        assert_eq!(results[0]["result"]["content"][0]["text"], "中文结果");
        assert_eq!(results[0]["result"]["structuredContent"]["count"], 1);
        assert_eq!(results[1]["ok"], true);
        assert_eq!(results[2]["ok"], false);
        assert_eq!(results[2]["result"]["isError"], true);
        if protocol == "completion" {
            final_reply(request, "完成");
        } else {
            response_final(request, "完成");
        }
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("mcp__local__echo"));
        assert!(stderr.contains("MCP: local connected (") && stderr.contains(") ✓"));
        assert!(stderr.contains("Connected to MCP servers: 1 succeeded ("));
        assert!(!stderr.contains("secret-"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("written")).unwrap(),
            "ok"
        );
        let requests = log(dir.path(), "mcp-log");
        assert_eq!(requests[0]["method"], "initialize");
        assert_eq!(requests[0]["params"]["capabilities"], json!({}));
        assert_eq!(requests[1]["method"], "notifications/initialized");
        assert_eq!(requests[3]["params"]["cursor"], "page-2");
        assert_eq!(requests[4]["params"]["name"], "echo");
        assert_eq!(requests[4]["params"]["arguments"], args);
        assert_eq!(requests[5]["result"], json!({}));
        assert_eq!(requests[6]["error"]["code"], -32601);
        assert_stopped(dir.path());
        assert!(dir.path().join("mcp-log.closed").is_file());
    }
}

#[test]
fn mcp_management_updates_both_protocols_and_reload_failure_preserves_tools() {
    let protocol = "response";
    let provider = server();
    let home = tempfile::tempdir().unwrap();
    let mut configured = settings("");
    configured["mcpServers"]["broken"] = json!({"command":"/nonexistent/pp-mcp-fixture"});
    // 与 `local` 共用 fixture，但写入独立日志与返回值，便于区分两条连接。
    let mut extra = configured["mcpServers"]["local"].clone();
    extra["env"]["MCP_LOG"] = json!("extra-log");
    extra["env"]["MCP_VALUE"] = json!("外部新增结果");
    let mut cli = Cli::start_with(
        home.path(),
        None,
        &configured.to_string(),
        &response_models(&provider, Some(protocol)),
    );
    let api = if protocol == "response" {
        "/v1/responses"
    } else {
        "/v1/chat/completions"
    };
    // 索引 3 只改 mcp.json、不发 `/mcp reload`：同一行模式会话必须沿用内存中的旧定义。
    // 索引 4 的 reload 才接入 `extra`；索引 5 用解析失败的配置验证旧工具保留。
    for (index, (command, expected)) in [
        ("/mcp list\n/mcp disable local", None),
        ("/mcp enable local", Some("mcp__local__echo")),
        ("/mcp reconnect local", Some("mcp__local__echo")),
        ("", Some("mcp__local__echo")),
        ("/mcp reload", Some("mcp__local__tool_")),
        ("/mcp reload", Some("mcp__local__tool_")),
    ]
    .into_iter()
    .enumerate()
    {
        if index == 3 {
            let mut changed = configured.clone();
            changed["mcpServers"]["extra"] = extra.clone();
            std::fs::write(
                config_path(home.path(), "mcp.json"),
                json!({"mcpServers":changed["mcpServers"]}).to_string(),
            )
            .unwrap();
        }
        if index == 4 {
            let mut changed = settings("special_name");
            changed["mcpServers"]["local"]["env"]["MCP_VALUE"] = json!("updated");
            changed["mcpServers"]["extra"] = extra.clone();
            std::fs::write(
                config_path(home.path(), "mcp.json"),
                json!({"mcpServers":changed["mcpServers"]}).to_string(),
            )
            .unwrap();
        }
        if index == 5 {
            std::fs::write(config_path(home.path(), "mcp.json"), "broken secret-config").unwrap();
        }
        cli.input(&format!("{command}\ntask {index}\n"));
        let (request, body) = receive_at(&provider, api);
        let definitions = body["tools"].to_string();
        assert!(!definitions.contains("mcp__broken__"));
        match expected {
            Some(name) => assert!(definitions.contains(name), "{index}: {definitions}"),
            None => assert!(!definitions.contains("mcp__local__")),
        }
        match index {
            // 外部新增的服务只有在 `/mcp reload` 后才进入工具表，`local` 也保持旧定义。
            3 => {
                assert!(
                    !definitions.contains("mcp__extra__"),
                    "{index}: {definitions}"
                );
                assert!(
                    !definitions.contains("mcp__local__tool_"),
                    "外部配置不应在 reload 前改写 local 定义: {index}: {definitions}"
                );
            }
            // reload 成功接入 `extra`；随后解析失败的 reload 保留旧工具。
            4 | 5 => assert!(
                definitions.contains("mcp__extra__echo"),
                "{index}: {definitions}"
            ),
            _ => {}
        }
        if protocol == "response" {
            response_final(request, &format!("done {index}"));
        } else {
            final_reply(request, &format!("done {index}"));
        }
    }
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Configured MCP Servers"));
    assert!(!stderr.contains("secret-config"));
    assert_stopped(home.path());
    stopped(home.path(), "extra-log");
    assert!(
        home.path().join("extra-log.closed").is_file(),
        "extra MCP 子进程未写入退出标记"
    );
    let journal = std::fs::read_dir(config_path(home.path(), "sessions"))
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
        .collect::<String>();
    assert!(!journal.contains("/mcp"));
}
