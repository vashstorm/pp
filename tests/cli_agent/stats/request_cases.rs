use super::*;
use std::fs;

#[test]
fn stats_context_first_request_freezes_full_body_skills_and_session_identity() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let base = format!(
            "# Long system prompt\n{}\n<script>marker & raw</script>\n\tcode\n",
            "完整内容 full context\n".repeat(5000)
        );
        fs::create_dir_all(dir.path().join(".pp")).unwrap();
        fs::write(dir.path().join(".pp/SYSTEM.md"), &base).unwrap();
        fs::write(dir.path().join("AGENTS.md"), "original-project-rule").unwrap();
        for name in ["available", "disabled"] {
            let skill = config_path(dir.path(), "skills").join(name);
            fs::create_dir_all(&skill).unwrap();
            fs::write(skill.join("SKILL.md"), format!("---\nname: {name}\ndescription: 'Header <tag> & details'\n---\nprivate skill body not yet read\n")).unwrap();
        }
        let mut cli = Cli::start_with(
            dir.path(),
            None,
            "default_model: fixture/primary\ndisabled_skills: [disabled]\n",
            &response_models(&server, Some(protocol)),
        );
        let endpoint = if protocol == "completion" {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        };
        let finish = |request, text| {
            if protocol == "completion" {
                final_reply(request, text);
            } else {
                response_final(request, text);
            }
        };
        cli.input("first user <tag> & input\n");
        let (request, first) = receive_at(&server, endpoint);
        let system = if protocol == "completion" {
            &first["messages"][0]["content"]
        } else {
            &first["instructions"]
        };
        assert!(system.as_str().unwrap().starts_with(&base));
        assert!(system.as_str().unwrap().contains("original-project-rule"));
        assert!(system.as_str().unwrap().contains("skill://available"));
        assert!(
            !system
                .as_str()
                .unwrap()
                .contains("private skill body not yet read")
        );
        finish(request, "first done");
        fs::write(dir.path().join("AGENTS.md"), "changed-project-rule").unwrap();
        fs::remove_dir_all(config_path(dir.path(), "skills")).unwrap();
        cli.input("second user\n");
        let (request, second) = receive_at(&server, endpoint);
        assert!(second.to_string().contains("changed-project-rule"));
        assert!(!second.to_string().contains("original-project-rule"));
        finish(request, "second done");
        cli.input("/new\nthird user\n");
        let (request, third) = receive_at(&server, endpoint);
        finish(request, "third done");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let snapshots = contexts(dir.path());
        assert_eq!(snapshots.len(), 3);
        assert_eq!(snapshots[0]["request"], first);
        assert_eq!(
            snapshots[0]["skills"],
            json!([{"name":"available","description":"Header <tag> & details","uri":"skill://available"}])
        );
        assert!(snapshots[1].is_null());
        assert_eq!(snapshots[2]["request"], third);
        assert_eq!(snapshots[2]["skills"], snapshots[0]["skills"]);
        let records = rows(dir.path());
        let session = std::path::Path::new(records[0]["session_file"].as_str().unwrap())
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap();
        let cli = Cli::spawn(
            dir.path(),
            &["--resume", session, "-p", "resumed user"],
            &[],
        );
        let (request, _) = receive_at(&server, endpoint);
        finish(request, "resume done");
        assert!(cli.finish().status.success());
        assert!(contexts(dir.path())[3].is_null());
        let (_stats, address) = start_stats_server(dir.path());
        let (status, detail) = http(
            &address,
            &format!("/api/stats/requests/{}", records[0]["id"]),
            &address,
        );
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<Value>(&detail).unwrap()["request_context"],
            snapshots[0]
        );
        for endpoint in ["requests", "traces", "overview", "tools", "projects"] {
            let (status, body) = http(
                &address,
                &format!("/api/stats/{endpoint}?range=all"),
                &address,
            );
            assert_eq!(status, 200);
            assert!(!body.contains("original-project-rule"));
            assert!(!body.contains("first user <tag>"));
            assert!(!body.contains("request_context"));
            if endpoint == "requests" {
                let list: Value = serde_json::from_str(&body).unwrap();
                for row in list["requests"].as_array().unwrap() {
                    assert_eq!(
                        row["has_context"],
                        row["id"] == records[0]["id"] || row["id"] == records[2]["id"]
                    );
                }
            }
        }
        let (status, body) = http(
            &address,
            &format!("/api/stats/traces/{}", records[0]["id"]),
            &address,
        );
        assert_eq!(status, 200);
        assert!(!body.contains("original-project-rule"));
        assert!(!body.contains("request_context"));
        no_more(&server);
    }
}

#[test]
fn stats_chat_captures_metrics_first_request_context_and_tool_calls() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sample.txt"), "fixture contents").unwrap();
    let mut models: Value = serde_yaml::from_str(&fixture_models(&server)).unwrap();
    models["providers"]["fixture"]["models"]["primary"]["cost"] =
        json!({"input":2,"cacheRead":0.2,"cacheWrite":2.5,"output":8});
    let cli = Cli::start_with(
        dir.path(),
        Some("private input marker"),
        fixture_settings(),
        &serde_yaml::to_string(&models).unwrap(),
    );
    let (request, first_body) = receive(&server);
    calls_reply(
        request,
        json!([call("read-one", "read", json!({"path":"sample.txt"}))]),
    );
    let (request, _) = receive(&server);
    let mut stream = request.start_sse().unwrap();
    stream
        .event(&chat_chunk(
            json!({"role":"assistant","reasoning_content":"verified reasoning","content":"done"}),
            None,
        ))
        .unwrap();
    stream.event(&chat_chunk(json!({}), Some("stop"))).unwrap();
    stream.event(&json!({"id":"usage","object":"chat.completion.chunk","created":1,"model":"fixture-model","choices":[],"usage":{"prompt_tokens":120,"completion_tokens":15,"total_tokens":135,"prompt_tokens_details":{"cached_tokens":100,"cache_write_tokens":10}}})).unwrap();
    stream.frame("[DONE]").unwrap();
    stream.finish();
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records = rows(dir.path());
    assert_eq!(records.len(), 2);
    assert_eq!(
        contexts(dir.path()),
        vec![json!({"request":first_body,"skills":[]}), Value::Null]
    );
    let last = &records[1];
    assert_eq!(last["status"], "ok");
    assert_eq!(last["input"], 120);
    assert_eq!(last["output"], 15);
    assert_eq!(last["cache"], 100);
    assert_eq!(last["cache_write"], 10);
    assert!(last["ttft"].is_number());
    // 工具调用只落工具名与参数字符数，不再从回答正文反推。
    let calls = tools(dir.path());
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["request"], records[0]["id"]);
    assert_eq!(calls[0]["tool"], "read");
    assert!(calls[0]["args"].as_i64().unwrap() > 0);
    // 技能摘要头不含用户输入或凭据；回答正文与思考完全不入库。
    let skills = database(dir.path())
        .query_row(
            "SELECT group_concat(skills_json) FROM request_contexts",
            [],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
        .unwrap_or_default();
    assert!(!skills.contains("private input marker"));
    assert!(!skills.contains("fixture-key"));
    assert!(!skills.contains("verified reasoning"));
    std::fs::write(
        dir.path().join(".pp/stats/pricing.json"),
        "invalid ignored file",
    )
    .unwrap();
    let (_stats, address) = start_stats_server(dir.path());
    let (status, body) = http(&address, "/api/stats/traces?range=all", &address);
    assert_eq!(status, 200);
    assert!(!body.contains("verified reasoning"));
    let traces: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(traces["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(traces["sessions"][0]["requests"], 2);
    let (status, body) = http(
        &address,
        &format!("/api/stats/traces/{}", last["id"]),
        &address,
    );
    assert_eq!(status, 200);
    let trace: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(trace["requests"].as_array().unwrap().len(), 2);
    assert!(!body.contains("private input marker"));
    assert!(!body.contains("verified reasoning"));
    // 详情只返回指标行与首请求快照，正文与会话条目字段不再存在。
    let (status, body) = http(
        &address,
        &format!("/api/stats/requests/{}", last["id"]),
        &address,
    );
    assert_eq!(status, 200);
    let detail: Value = serde_json::from_str(&body).unwrap();
    assert!(detail.get("output_message").is_none());
    assert!(detail.get("session_entry").is_none());
    assert!(detail["request_context"].is_null());
    assert!(!body.contains("verified reasoning"));
    assert!(!body.contains("private input marker"));
    let (status, body) = http(&address, "/api/stats/errors?range=all", &address);
    assert_eq!(status, 200);
    let errors: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(errors["limit"], 50);
    assert!(errors["requests"].as_array().unwrap().is_empty());
    assert!(!body.contains("private input marker"));
    for endpoint in ["overview", "requests", "models", "providers", "costs"] {
        let (status, body) = http(
            &address,
            &format!("/api/stats/{endpoint}?range=all"),
            &address,
        );
        assert_eq!(status, 200);
        assert!(!body.contains("fixture-key"));
        let value: Value = serde_json::from_str(&body).unwrap();
        assert!(value.get("pricing_error").is_none());
        if endpoint != "requests" {
            assert_eq!(value["summary"]["priced"], 1);
            assert_eq!(value["summary"]["cache_write_tokens"], 10);
            assert!(
                (value["summary"]["cost_components"]["cache_write"]
                    .as_f64()
                    .unwrap()
                    - 0.000025)
                    .abs()
                    < 1e-10
            );
            assert!((value["summary"]["cost"].as_f64().unwrap() - 0.000185).abs() < 1e-10);
        }
    }
    // 费用在请求记录时写入快照：改写 models.yml 不重算历史。
    models["providers"]["fixture"]["models"]["primary"]["cost"] =
        json!({"input":4,"cacheRead":0.4,"cacheWrite":5,"output":16});
    std::fs::write(config_path(dir.path(), "models.yml"), models.to_string()).unwrap();
    let (status, body) = http(
        &address,
        &format!("/api/stats/requests/{}", last["id"]),
        &address,
    );
    assert_eq!(status, 200);
    let detail: Value = serde_json::from_str(&body).unwrap();
    assert!((detail["request"]["cost"].as_f64().unwrap() - 0.000185).abs() < 1e-10);
    assert!(
        (detail["request"]["cost_components"]["cache_write"]
            .as_f64()
            .unwrap()
            - 0.000025)
            .abs()
            < 1e-10
    );
}

#[test]
fn stats_tool_result_text_matches_model_context_in_both_protocols() {
    for responses in [false, true] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let models = if responses {
            response_models(&server, Some("response"))
        } else {
            fixture_models(&server)
        };
        let cli = Cli::start_with(
            dir.path(),
            Some("tool result text"),
            fixture_settings(),
            &models,
        );
        let endpoint = if responses {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        };
        let (request, _) = receive_at(&server, endpoint);
        let args = json!({"command":"printf '中😀'"});
        if responses {
            response_reply(
                request,
                json!([response_call("stats-result", "bash", args)]),
            );
        } else {
            calls_reply(request, json!([call("stats-result", "bash", args)]));
        }
        let (request, body) = receive_at(&server, endpoint);
        let result = if responses {
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["type"] == "function_call_output")
                .unwrap()["output"]
                .as_str()
                .unwrap()
        } else {
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["role"] == "tool")
                .unwrap()["content"]
                .as_str()
                .unwrap()
        };
        let chars = result.chars().count() as u64;
        assert!(result.len() > chars as usize);
        let value: Value = serde_json::from_str(result).unwrap();
        assert!(value.get("shell").is_none());
        if responses {
            response_final(request, "done");
        } else {
            final_reply(request, "done");
        }
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stored = database(dir.path())
            .query_row(
                "SELECT result_chars FROM tool_calls WHERE call_id='stats-result'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(stored as u64, chars);
        let (_stats, address) = start_stats_server(dir.path());
        let (status, body) = http(&address, "/api/stats/tools?range=all", &address);
        assert_eq!(status, 200);
        let data: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(data["summary"]["result_chars"], chars);
        assert_eq!(data["summary"]["result_calls"], 1);
        assert_eq!(data["tools"][0]["models"][0]["result_chars"], chars);
    }
}

#[test]
fn stats_tool_failures_are_recorded_and_served_by_tools() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let cli = Cli::start_with(
        dir.path(),
        Some("failing tool"),
        fixture_settings(),
        &fixture_models(&server),
    );
    let (request, _) = receive(&server);
    calls_reply(
        request,
        json!([call("read-missing", "read", json!({"path":"missing.txt"}))]),
    );
    let (request, body) = receive(&server);
    let result_chars = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap()["content"]
        .as_str()
        .unwrap()
        .chars()
        .count() as u64;
    final_reply(request, "recovered after tool failure");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let db = database(dir.path());
    let records: Vec<Value> = db
        .prepare(
            "SELECT tool,call_id,status,error_message,session_file,request_key,result_chars FROM tool_calls",
        )
        .unwrap()
        .query_map([], |row| {
            Ok(json!({
                "tool": row.get::<_, String>(0)?,
                "call_id": row.get::<_, String>(1)?,
                "status": row.get::<_, String>(2)?,
                "error": row.get::<_, Option<String>>(3)?,
                "session": row.get::<_, Option<String>>(4)?,
                "request_key": row.get::<_, String>(5)?,
                "result_chars": row.get::<_, i64>(6)?,
            }))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record["tool"], "read");
    assert_eq!(record["call_id"], "read-missing");
    assert_eq!(record["status"], "error");
    assert_eq!(record["result_chars"], result_chars);
    assert!(record["error"].as_str().unwrap().contains("missing.txt"));
    assert!(record["session"].as_str().unwrap().ends_with(".jsonl"));
    assert!(!record["request_key"].as_str().unwrap().is_empty());

    let (_stats, address) = start_stats_server(dir.path());
    // Errors 端点只承载失败请求；工具失败的展示入口是 Tools 页。
    let (status, body) = http(&address, "/api/stats/errors?range=all", &address);
    assert_eq!(status, 200);
    let errors: Value = serde_json::from_str(&body).unwrap();
    let mut keys = errors
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    assert_eq!(
        keys,
        ["has_more", "limit", "range", "requests", "updated_at"]
    );
    assert!(errors["requests"].as_array().unwrap().is_empty());
    let (status, body) = http(&address, "/api/stats/tools?range=all", &address);
    assert_eq!(status, 200);
    let tools: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(tools["summary"]["errors"], 1);
    assert_eq!(tools["summary"]["executions"], 1);
    assert_eq!(tools["summary"]["result_chars"], result_chars);
    assert_eq!(tools["summary"]["result_calls"], 1);
    let read = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["tool"] == "read")
        .unwrap();
    assert_eq!(read["calls"], 1);
    assert_eq!(read["errors"], 1);
    assert_eq!(read["executions"], 1);
    assert_eq!(read["models"][0]["errors"], 1);
    assert_eq!(read["result_chars"], result_chars);
    assert_eq!(read["models"][0]["result_chars"], result_chars);
    assert!(read["avg_duration_ms"].as_f64().is_some());
}
