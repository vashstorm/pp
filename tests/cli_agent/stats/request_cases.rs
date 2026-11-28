use super::*;

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
