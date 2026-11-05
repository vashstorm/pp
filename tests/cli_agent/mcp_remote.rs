use super::*;
#[path = "../fixtures/mcp_remote.rs"]
mod fixture;

#[test]
fn mcp_remote_both_model_protocols_and_transports_preserve_results_and_auth_precedence() {
    for protocol in ["completion", "response"] {
        for (transport, mode) in [("http", "json"), ("http", "sse"), ("sse", "json")] {
            let remote = fixture::RemoteServer::new(mode);
            let provider = server();
            let dir = tempfile::tempdir().unwrap();
            let mut config = remote.config(transport, 2000);
            config["mcpServers"]["remote"]["env_headers"] = json!({"x-test-key":"PP_MCP_TEST_KEY"});
            write_settings(dir.path(), config.to_string()).unwrap();
            std::fs::write(
                config_path(dir.path(), "models.yml"),
                response_models(&provider, Some(protocol)),
            )
            .unwrap();
            let cli = Cli::start_env(
                dir.path(),
                dir.path(),
                &["-p", "调用远程工具"],
                &[("PP_MCP_TEST_KEY", "env-secret")],
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
                .map(|tool| {
                    if protocol == "completion" {
                        &tool["function"]
                    } else {
                        tool
                    }
                })
                .find(|tool| tool["name"] == "mcp__remote__echo")
                .unwrap_or_else(|| {
                    panic!(
                        "未发现远程工具 {transport}/{mode}/{protocol}，RPC 方法：{:?}",
                        remote
                            .log()
                            .iter()
                            .map(|r| (&r["method"], &r["body"]["method"]))
                            .collect::<Vec<_>>()
                    )
                });
            assert_eq!(echo["description"], "远程测试工具");
            assert_eq!(echo["strict"], false);
            assert_eq!(echo["parameters"]["required"], json!(["text"]));
            let args = json!({"text":"中文远程结果"});
            if protocol == "completion" {
                calls_reply(
                    request,
                    json!([
                        call("remote-1", "mcp__remote__echo", args),
                        call("remote-2", "mcp__remote__fail", json!({}))
                    ]),
                );
            } else {
                response_reply(
                    request,
                    json!([
                        response_call("remote-1", "mcp__remote__echo", args),
                        response_call("remote-2", "mcp__remote__fail", json!({}))
                    ]),
                );
            }
            let (request, body) = receive_at(&provider, path);
            let items = if protocol == "completion" {
                &body["messages"]
            } else {
                &body["input"]
            };
            let outputs: Vec<_> = items
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["role"] == "tool" || item["type"] == "function_call_output")
                .collect();
            let results: Vec<_> = outputs
                .iter()
                .zip(["remote-1", "remote-2"])
                .map(|(item, id)| {
                    if protocol == "completion" {
                        result(item, id)
                    } else {
                        response_result(item, id)
                    }
                })
                .collect();
            assert_eq!(
                results[0]["ok"], true,
                "{transport}/{mode}/{protocol}: {}",
                results[0]
            );
            assert_eq!(results[0]["result"]["content"][0]["text"], "中文远程结果");
            assert_eq!(results[0]["result"]["structuredContent"]["count"], 1);
            assert_eq!(results[1]["ok"], false);
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
            assert!(!String::from_utf8_lossy(&output.stderr).contains("secret"));
            assert!(
                remote
                    .log()
                    .iter()
                    .all(|r| r["headers"]["x-test-key"] == "env-secret")
            );
            let saved = std::fs::read_to_string(config_path(dir.path(), "mcp.json")).unwrap();
            assert!(!saved.contains("env-secret"));
            assert!(saved.contains("literal-secret"));
        }
    }
}

#[test]
fn mcp_remote_management_both_protocols_reload_refreshes_auth_without_persisting_secrets() {
    for protocol in ["completion", "response"] {
        for transport in ["http", "sse"] {
            let remote = fixture::RemoteServer::new("json");
            let provider = server();
            let dir = tempfile::tempdir().unwrap();
            let mut config = remote.config(transport, 3000);
            config["mcpServers"]["remote"]["env_headers"] = json!({"x-test-key":"PP_MCP_TEST_KEY"});
            write_settings(dir.path(), config.to_string()).unwrap();
            std::fs::write(
                config_path(dir.path(), "models.yml"),
                response_models(&provider, Some(protocol)),
            )
            .unwrap();
            let mut cli = Cli::start_env(
                dir.path(),
                dir.path(),
                &[],
                &[
                    ("PP_MCP_TEST_KEY", "env-secret"),
                    ("PP_MCP_TEST_KEY_NEW", "refreshed-secret"),
                ],
            );
            let api = if protocol == "completion" {
                "/v1/chat/completions"
            } else {
                "/v1/responses"
            };
            for (index, command) in [
                "/mcp disable remote",
                "/mcp enable remote",
                "/mcp reconnect remote",
                "/mcp reload",
            ]
            .iter()
            .enumerate()
            {
                if index == 2 {
                    remote.changed();
                }
                if index == 3 {
                    config["mcpServers"]["remote"]["env_headers"] =
                        json!({"x-test-key":"PP_MCP_TEST_KEY_NEW"});
                    std::fs::write(config_path(dir.path(), "mcp.json"), config.to_string())
                        .unwrap();
                }
                cli.input(&format!("{command}\ntask {index}\n"));
                let (request, body) = receive_at(&provider, api);
                let tools = body["tools"].to_string();
                assert_eq!(tools.contains("mcp__remote__echo"), index != 0);
                if index >= 2 {
                    assert!(tools.contains("changed"));
                }
                if index == 3 {
                    if protocol == "completion" {
                        calls_reply(
                            request,
                            json!([call(
                                "reloaded",
                                "mcp__remote__echo",
                                json!({"text":"updated"})
                            )]),
                        );
                    } else {
                        response_reply(
                            request,
                            json!([response_call(
                                "reloaded",
                                "mcp__remote__echo",
                                json!({"text":"updated"})
                            )]),
                        );
                    }
                    let (request, body) = receive_at(&provider, api);
                    assert!(body.to_string().contains("updated"));
                    remote.wait_calls(1);
                    if protocol == "completion" {
                        final_reply(request, "done");
                    } else {
                        response_final(request, "done");
                    }
                } else if protocol == "completion" {
                    final_reply(request, "done");
                } else {
                    response_final(request, "done");
                }
            }
            cli.input("/quit\n");
            let output = cli.finish();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let log = remote.log();
            let call = log
                .iter()
                .find(|r| r["body"]["method"] == "tools/call")
                .unwrap();
            assert_eq!(call["headers"]["x-test-key"], "refreshed-secret");
            let saved = std::fs::read_to_string(config_path(dir.path(), "mcp.json")).unwrap();
            assert!(!saved.contains("env-secret") && !saved.contains("refreshed-secret"));
            assert!(saved.contains("literal-secret"));
        }
    }
}
