use super::*;

pub(super) fn settings(mode: &str) -> Value {
    json!({"mcp_servers":{"local":{
        "command":"bash",
        "args":[format!("{}/tests/fixtures/mcp_server.sh", env!("CARGO_MANIFEST_DIR"))],
        "env":{"MCP_MODE":mode,"MCP_VALUE":"中文结果","MCP_LOG":"mcp-log"},
        "timeout_seconds":1
    }}})
}

fn log(dir: &std::path::Path, name: &str) -> Vec<Value> {
    std::fs::read_to_string(dir.join(name))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

pub(super) fn assert_stopped(dir: &std::path::Path) {
    let pid: i32 = std::fs::read_to_string(dir.join("mcp-log.pid"))
        .unwrap()
        .parse()
        .unwrap();
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    assert!(
        rustix::process::test_kill_process(pid).is_err(),
        "MCP 子进程未清理"
    );
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
fn mcp_startup_failures_are_bounded_redacted_and_reap_processes() {
    for mode in [
        "bad_version",
        "invalid_json",
        "wrong_id",
        "eof",
        "init_error",
        "timeout_init",
        "oversized",
        "cursor_loop",
        "duplicate",
        "invalid_schema",
    ] {
        let provider = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            dir.path(),
            Some("test"),
            &settings(mode).to_string(),
            &fixture_models(&provider),
        );
        let output = cli.finish();
        assert!(!output.status.success(), "{mode}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("MCP"), "{mode}: {stderr}");
        assert!(!stderr.contains("secret-"), "{mode}: {stderr}");
        no_more(&provider);
        assert_stopped(dir.path());
    }
}

#[test]
fn mcp_call_failures_reach_model_and_failed_connections_are_not_reused() {
    for mode in [
        "call_timeout",
        "call_eof",
        "call_bad_json",
        "rpc_error",
        "blocked_write",
    ] {
        let provider = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            dir.path(),
            Some("test"),
            &settings(mode).to_string(),
            &fixture_models(&provider),
        );
        let (request, _) = receive(&provider);
        let args = if mode == "blocked_write" {
            json!({"text":"x".repeat(262144)})
        } else {
            json!({"text":"ok"})
        };
        calls_reply(
            request,
            json!([
                call("one", "mcp__local__echo", args),
                call("two", "mcp__local__echo", json!({"text":"ok"}))
            ]),
        );
        // 大参数会通过 CLI 工具事件写入 stderr，使用已有的双管道 drain 避免测试本身阻塞。
        let live = super::streaming::LiveCli::new(cli);
        let (request, body) = receive(&provider);
        let messages = body["messages"].as_array().unwrap();
        let results: Vec<_> = messages
            .iter()
            .filter(|item| item["role"] == "tool")
            .collect();
        let first = result(results[0], "one");
        let second = result(results[1], "two");
        assert_eq!(first["ok"], false, "{mode}");
        assert_eq!(second["ok"], mode == "rpc_error", "{mode}");
        assert!(!first.to_string().contains("secret-"));
        final_reply(request, "recovered");
        let output = live.finish();
        assert!(output.status.success());
        assert_stopped(dir.path());
    }
}

#[test]
fn mcp_config_rejects_invalid_fields_types_and_values_without_leaking_secrets() {
    let cases = [
        json!({"command":"secret-command","unknown":"secret-config"}),
        json!({"command":""}),
        json!({"command":"bash","args":"secret-config"}),
        json!({"command":"bash","env":{"A":null}}),
        json!({"command":"bash","env":{"A=B":"secret-config"}}),
        json!({"command":"bash","timeout_seconds":0}),
        json!({"command":"bash","timeout_seconds":null}),
        json!({"command":"bash","timeout_seconds":-1}),
        json!({"command":"bash","timeout_seconds":1.5}),
        json!({"command":"bash","timeout_seconds":18446744073709551615_u64}),
    ];
    for config in cases {
        let provider = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            dir.path(),
            Some("test"),
            &json!({"mcp_servers":{"local":config}}).to_string(),
            &fixture_models(&provider),
        );
        let output = cli.finish();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-"));
        no_more(&provider);
    }
}

#[test]
fn mcp_connections_survive_model_switch_and_reset_and_refresh_on_resume() {
    let provider = server();
    let home = tempfile::tempdir().unwrap();
    let models = json!({"providers":[
        slash_provider(&provider,"a","completion",json!([{"id":"m","model":"a"}])),
        slash_provider(&provider,"b","response",json!([{"id":"m","model":"b"}]))
    ]})
    .to_string();
    let mut cli = Cli::start_with(home.path(), None, &settings("").to_string(), &models);
    for (index, command) in [
        "first\n",
        "/model b/m\nsecond\n",
        "/new\n/model a/m\nthird\n",
    ]
    .iter()
    .enumerate()
    {
        cli.input(command);
        let response = index == 1;
        let path = if response {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        };
        let (request, body) = receive_at(&provider, path);
        assert!(body["tools"].to_string().contains("mcp__local__echo"));
        if response {
            response_reply(
                request,
                json!([response_call(
                    "echo",
                    "mcp__local__echo",
                    json!({"text":"ok"})
                )]),
            );
        } else {
            calls_reply(
                request,
                json!([call("echo", "mcp__local__echo", json!({"text":"ok"}))]),
            );
        }
        let (request, body) = receive_at(&provider, path);
        let item = if response {
            body["input"].as_array().unwrap().last().unwrap()
        } else {
            body["messages"].as_array().unwrap().last().unwrap()
        };
        let output = if response {
            response_result(item, "echo")
        } else {
            result(item, "echo")
        };
        assert_eq!(output["result"]["structuredContent"]["count"], index + 1);
        if response {
            response_final(request, "done");
        } else {
            final_reply(request, "done");
        }
    }
    cli.input("/quit\n");
    assert!(cli.finish().status.success());
    assert_stopped(home.path());
    let requests = log(home.path(), "mcp-log");
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["method"] == "initialize")
            .count(),
        1
    );
    let sessions = config_path(home.path(), "sessions");
    let mut ids: Vec<_> = std::fs::read_dir(&sessions)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    ids.sort_by_key(|path| std::fs::metadata(path).unwrap().modified().unwrap());
    let id = ids.last().unwrap().file_stem().unwrap().to_str().unwrap();
    let mut changed = settings("");
    changed["mcp_servers"]["local"]["env"]["MCP_VALUE"] = json!("updated");
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        changed.to_string(),
    )
    .unwrap();
    let cli = Cli::spawn(home.path(), &["-r", id, "-p", "resumed"], &[]);
    let (request, body) = receive(&provider);
    assert!(body["tools"].to_string().contains("mcp__local__echo"));
    calls_reply(
        request,
        json!([call("resumed", "mcp__local__echo", json!({"text":"ok"}))]),
    );
    let (request, body) = receive(&provider);
    let output = result(
        body["messages"].as_array().unwrap().last().unwrap(),
        "resumed",
    );
    assert_eq!(output["result"]["content"][0]["text"], "updated");
    assert_eq!(output["result"]["structuredContent"]["count"], 1);
    final_reply(request, "resumed");
    assert!(cli.finish().status.success());
    assert_stopped(home.path());
    std::fs::write(config_path(home.path(), "settings.yml"), "{}").unwrap();
    let cli = Cli::spawn(home.path(), &["-r", id, "-p", "removed"], &[]);
    let (request, body) = receive(&provider);
    assert!(!body["tools"].to_string().contains("mcp__"));
    calls_reply(
        request,
        json!([call("removed", "mcp__local__echo", json!({}))]),
    );
    let (request, body) = receive(&provider);
    assert_eq!(
        result(
            body["messages"].as_array().unwrap().last().unwrap(),
            "removed"
        )["ok"],
        false
    );
    final_reply(request, "removed");
    assert!(cli.finish().status.success());
}

#[test]
fn mcp_multiple_servers_and_optional_capabilities_keep_names_separate() {
    for mode in ["no_tools", "old_version"] {
        let provider = server();
        let dir = tempfile::tempdir().unwrap();
        let mut config = settings(mode);
        let mut second = settings("")["mcp_servers"]["local"].clone();
        second["env"]["MCP_LOG"] = json!("second-log");
        config["mcp_servers"]["second"] = second;
        let cli = Cli::start_with(
            dir.path(),
            Some("test"),
            &config.to_string(),
            &fixture_models(&provider),
        );
        let (request, body) = receive(&provider);
        let definitions = body["tools"].to_string();
        assert_eq!(definitions.contains("mcp__local__echo"), mode != "no_tools");
        assert!(definitions.contains("mcp__second__echo"));
        calls_reply(
            request,
            json!([
                call("valid", "mcp__second__echo", json!({"text":"ok"})),
                call("bad", "mcp__second__echo", json!([])),
                call("unknown", "mcp__missing__echo", json!({}))
            ]),
        );
        let (request, body) = receive(&provider);
        let outputs: Vec<_> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["role"] == "tool")
            .collect();
        assert_eq!(result(outputs[0], "valid")["ok"], true);
        assert_eq!(result(outputs[1], "bad")["ok"], false);
        assert_eq!(result(outputs[2], "unknown")["ok"], false);
        final_reply(request, "done");
        assert!(cli.finish().status.success());
        let requests = log(dir.path(), "mcp-log");
        assert_eq!(
            requests.iter().any(|r| r["method"] == "tools/list"),
            mode != "no_tools"
        );
        assert_stopped(dir.path());
    }
}

#[test]
fn mcp_special_and_long_tool_names_are_legal_stable_and_route_to_original_names() {
    for mode in ["special_name", "long_name"] {
        let provider = server();
        let dir = tempfile::tempdir().unwrap();
        let mut previous = None;
        for _ in 0..2 {
            let cli = Cli::start_with(
                dir.path(),
                Some("test"),
                &settings(mode).to_string(),
                &fixture_models(&provider),
            );
            let (request, body) = receive(&provider);
            let tools = body["tools"].as_array().unwrap();
            let name = tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str())
                .find(|name| name.starts_with("mcp__local__tool_"))
                .unwrap()
                .to_owned();
            assert!(name.len() <= 64);
            assert!(
                name.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            );
            if let Some(previous) = &previous {
                assert_eq!(&name, previous);
            }
            previous = Some(name.clone());
            calls_reply(
                request,
                json!([call("encoded", &name, json!({"text":"ok"}))]),
            );
            let (request, body) = receive(&provider);
            assert_eq!(
                result(
                    body["messages"].as_array().unwrap().last().unwrap(),
                    "encoded"
                )["ok"],
                true
            );
            final_reply(request, "done");
            assert!(cli.finish().status.success());
        }
        let requests = log(dir.path(), "mcp-log");
        let calls: Vec<_> = requests
            .iter()
            .filter(|request| request["method"] == "tools/call")
            .collect();
        let original = if mode == "special_name" {
            "文件.search".into()
        } else {
            "0".repeat(80)
        };
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|call| call["params"]["name"] == original));
    }
}
