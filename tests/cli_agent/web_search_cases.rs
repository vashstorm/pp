use super::*;

#[test]
fn web_search_roundtrip_both_protocols_and_sandbox_network_denial() {
    for protocol in ["completion", "response"] {
        let llm_server = server();
        let search_server = server();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let settings = format!(
            "rate_limit_retry_budget_seconds: 0\nwebSearch:\n  provider: searxng\n  searxng:\n    endpoint: http://{}\n",
            search_server.addr()
        );
        let models = if protocol == "completion" {
            fixture_models(&llm_server)
        } else {
            response_models(&llm_server, Some("response"))
        };
        let cli = Cli::start_with(&root, Some("search rust"), &settings, &models);

        let search_worker = {
            let srv = Arc::clone(&search_server);
            thread::spawn(move || {
                let req = srv.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
                assert!(req.target().starts_with("/search?"), "{}", req.target());
                req.respond(
                    Response::builder()
                        .header("Content-Type", "application/json")
                        .body(Body::from(
                            r#"{"results":[{"title":"Rust Lang","url":"https://www.rust-lang.org/","content":"A systems programming language"}]}"#,
                        ))
                        .unwrap(),
                )
                .unwrap();
            })
        };

        if protocol == "completion" {
            let (req, body) = receive_at(&llm_server, "/v1/chat/completions");
            assert!(
                body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["function"]["name"] == "web_search")
            );
            calls_reply(
                req,
                json!([call("ws-1", "web_search", json!({"query": "rust"}))]),
            );
            let (req, body) = receive_at(&llm_server, "/v1/chat/completions");
            let tool_msg = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["role"] == "tool")
                .unwrap();
            let res: Value = serde_json::from_str(tool_msg["content"].as_str().unwrap()).unwrap();
            assert_eq!(res["ok"], true, "{res}");
            assert_eq!(res["provider"], "searxng");
            assert!(
                res["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("https://www.rust-lang.org/")),
                "{res}"
            );
            final_reply(req, "found rust-lang.org");
        } else {
            let (req, body) = receive_at(&llm_server, "/v1/responses");
            assert!(
                body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["name"] == "web_search")
            );
            response_reply(
                req,
                json!([response_call(
                    "ws-1",
                    "web_search",
                    json!({"query": "rust"})
                )]),
            );
            let (req, body) = receive_at(&llm_server, "/v1/responses");
            let output_item = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["type"] == "function_call_output")
                .unwrap();
            let res: Value = serde_json::from_str(output_item["output"].as_str().unwrap()).unwrap();
            assert_eq!(res["ok"], true, "{res}");
            assert_eq!(res["provider"], "searxng");
            response_reply(
                req,
                json!([response_message(
                    "msg-1",
                    json!([response_text("found rust-lang.org")])
                )]),
            );
        }

        search_worker.join().unwrap();
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("found rust-lang.org"));
        no_more(&llm_server);
        no_more(&search_server);
    }

    // 在支持的平台上开启 network: false 沙盒时，外部引擎（duckduckgo）必须被拒绝。
    let probe = pp_sandbox::Sandbox::new(
        pp_config::config::SandboxSettings {
            mode: pp_config::config::SandboxMode::ReadOnly,
            network: false,
            ..Default::default()
        },
        std::path::Path::new("."),
    );
    if probe.status().effective != pp_sandbox::EffectiveMode::Off {
        let llm_server = server();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_settings(
            &root,
            "rate_limit_retry_budget_seconds: 0\ntools:\n  sandbox:\n    mode: read-only\n    network: false\n",
        )
        .unwrap();
        std::fs::write(
            config_path(&root, "models.yml"),
            fixture_models(&llm_server),
        )
        .unwrap();
        let cli = Cli::spawn(&root, &["-p", "search blocked"], &[]);
        let (req, _) = receive_at(&llm_server, "/v1/chat/completions");
        calls_reply(
            req,
            json!([call("ws-deny", "web_search", json!({"query": "rust"}))]),
        );
        let (req, body) = receive_at(&llm_server, "/v1/chat/completions");
        let tool_msg = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .unwrap();
        let res: Value = serde_json::from_str(tool_msg["content"].as_str().unwrap()).unwrap();
        assert_eq!(res["ok"], false, "{res}");
        assert!(
            res["error"].as_str().is_some_and(|e| e.contains("sandbox")),
            "{res}"
        );
        final_reply(req, "blocked-ok");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        no_more(&llm_server);
    }
}
