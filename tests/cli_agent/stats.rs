use super::*;
use rusqlite::Connection;

pub(super) fn database(home: &std::path::Path) -> Connection {
    Connection::open(home.join(".pp/stats/stats.db")).unwrap()
}
fn rows(home: &std::path::Path) -> Vec<Value> {
    database(home).prepare("SELECT r.id,r.status,r.input_tokens,r.output_tokens,r.cached_tokens,r.ttft_ms,r.purpose,r.entry_id,r.session_file,p.output_json,p.session_entry_json,r.error_message FROM requests r JOIN request_payloads p ON p.request_id=r.id ORDER BY r.id").unwrap().query_map([],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"status":r.get::<_,String>(1)?,"input":r.get::<_,Option<i64>>(2)?,"output":r.get::<_,Option<i64>>(3)?,"cache":r.get::<_,Option<i64>>(4)?,"ttft":r.get::<_,Option<f64>>(5)?,"purpose":r.get::<_,String>(6)?,"entry_id":r.get::<_,Option<String>>(7)?,"session_file":r.get::<_,Option<String>>(8)?,"payload":serde_json::from_str::<Value>(&r.get::<_,String>(9)?).unwrap(),"entry":r.get::<_,Option<String>>(10)?.map(|s|serde_json::from_str::<Value>(&s).unwrap()),"error":r.get::<_,Option<String>>(11)?}))).unwrap().collect::<Result<_,_>>().unwrap()
}

#[test]
fn stats_chat_captures_usage_thinking_tools_and_real_session_entries() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sample.txt"), "fixture contents").unwrap();
    let mut models: Value = serde_yaml::from_str(&fixture_models(&server)).unwrap();
    models["providers"][0]["models"][0]["cost"] = json!({"input":2,"cacheRead":0.2,"output":8});
    let cli = Cli::start_with(
        dir.path(),
        Some("private input marker"),
        fixture_settings(),
        &serde_yaml::to_string(&models).unwrap(),
    );
    let (request, _) = receive(&server);
    calls_reply(
        request,
        json!([call("read-one", "read", json!({"path":"sample.txt"}))]),
    );
    let (request, _) = receive(&server);
    let mut stream = SseReply::new(request);
    stream.event(chat_chunk(
        json!({"role":"assistant","reasoning_content":"verified reasoning","content":"done"}),
        None,
    ));
    stream.event(chat_chunk(json!({}), Some("stop")));
    stream.event(json!({"id":"usage","object":"chat.completion.chunk","created":1,"model":"fixture-model","choices":[],"usage":{"prompt_tokens":120,"completion_tokens":15,"total_tokens":135,"prompt_tokens_details":{"cached_tokens":100}}}));
    stream.frame("[DONE]");
    stream.finish();
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records = rows(dir.path());
    assert_eq!(records.len(), 2);
    let last = &records[1];
    assert_eq!(last["status"], "ok");
    assert_eq!(last["input"], 120);
    assert_eq!(last["output"], 15);
    assert_eq!(last["cache"], 100);
    assert!(last["ttft"].is_number());
    assert_eq!(last["payload"]["thinking"], "verified reasoning");
    assert_eq!(last["entry"][0]["id"], last["entry_id"]);
    assert_eq!(
        records[0]["payload"]["native"]["tool_calls"][0]["function"]["name"],
        "read"
    );
    let session = std::fs::read_to_string(last["session_file"].as_str().unwrap()).unwrap();
    for record in &records {
        for entry in record["entry"].as_array().unwrap() {
            assert!(
                session
                    .lines()
                    .any(|line| serde_json::from_str::<Value>(line).ok().as_ref() == Some(entry))
            );
        }
    }
    let payloads = records
        .iter()
        .map(|r| r["payload"].to_string())
        .collect::<String>();
    assert!(!payloads.contains("private input marker"));
    assert!(!payloads.contains("fixture-key"));
    std::fs::write(
        dir.path().join(".pp/stats/pricing.json"),
        "invalid ignored file",
    )
    .unwrap();
    let (_stats, address) = start_stats_server(dir.path());
    for endpoint in ["overview", "requests", "models", "providers", "costs"] {
        let (status, body) = http(
            &address,
            &format!("/api/stats/{endpoint}?range=all"),
            &address,
        );
        assert_eq!(status, 200);
        assert!(!body.contains("fixture-key"));
        let value: Value = serde_json::from_str(&body).unwrap();
        assert!(value["pricing_error"].is_null());
        if endpoint != "requests" {
            assert_eq!(value["summary"]["priced"], 1);
            assert!((value["summary"]["cost"].as_f64().unwrap() - 0.00018).abs() < 1e-10);
        }
    }
    models["providers"][0]["models"][0]["cost"] = json!({"input":4,"cacheRead":0.4,"output":16});
    std::fs::write(config_path(dir.path(), "models.yml"), models.to_string()).unwrap();
    let (status, body) = http(
        &address,
        &format!("/api/stats/requests/{}", last["id"]),
        &address,
    );
    assert_eq!(status, 200);
    let detail: Value = serde_json::from_str(&body).unwrap();
    assert!((detail["request"]["cost"].as_f64().unwrap() - 0.00036).abs() < 1e-10);
}

#[test]
fn stats_responses_captures_real_usage_and_keeps_unknown_cache_unknown() {
    for cached in [Some(40), None] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            dir.path(),
            Some("stats response"),
            fixture_settings(),
            &response_models(&server, Some("response")),
        );
        let (request, _) = receive_at(&server, "/v1/responses");
        let output = json!([response_message(
            "msg-stats",
            json!([{"type":"output_text","text":"response done","annotations":[]}])
        )]);
        let mut body = response_body(output);
        if let Some(value) = cached {
            body["usage"] = json!({"input_tokens":50,"output_tokens":10,"total_tokens":60,"input_tokens_details":{"cached_tokens":value},"output_tokens_details":{"reasoning_tokens":0}});
        }
        let mut stream = SseReply::new(request);
        stream.event(json!({"type":"response.output_text.delta","sequence_number":0,"item_id":"msg-stats","output_index":0,"content_index":0,"delta":"response done","logprobs":[]}));
        stream.event(json!({"type":"response.completed","sequence_number":1,"response":body}));
        stream.finish();
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let records = rows(dir.path());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["cache"], json!(cached));
        assert_eq!(
            records[0]["input"],
            if cached.is_some() {
                json!(50)
            } else {
                Value::Null
            }
        );
        assert_eq!(
            records[0]["output"],
            if cached.is_some() {
                json!(10)
            } else {
                Value::Null
            }
        );
        assert_eq!(records[0]["entry"][0]["id"], records[0]["entry_id"]);
        assert!(records[0]["ttft"].is_number());
    }
}

#[test]
fn stats_http_errors_are_counted_once_without_response_body_or_credentials() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            dir.path(),
            Some("stats failure"),
            fixture_settings(),
            &response_models(&server, Some(protocol)),
        );
        let (request, _) = receive_at(
            &server,
            if protocol == "completion" {
                "/v1/chat/completions"
            } else {
                "/v1/responses"
            },
        );
        request
            .respond(Response::from_string("private-error-body fixture-key").with_status_code(429))
            .unwrap();
        let output = cli.finish();
        assert!(!output.status.success());
        let records = rows(dir.path());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["status"], "failed");
        assert!(records[0]["entry"].is_null());
        let encoded = records[0].to_string();
        assert!(!encoded.contains("private-error-body"));
        assert!(!encoded.contains("fixture-key"));
        assert!(encoded.contains("429"));
        no_more(&server);
    }
}

#[test]
fn stats_unavailable_database_does_not_fail_model_task() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".pp")).unwrap();
    std::fs::write(dir.path().join(".pp/stats"), "blocked directory").unwrap();
    let cli = Cli::start(&server, dir.path(), Some("stats unavailable"));
    let (request, _) = receive(&server);
    final_reply(request, "task still completed");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"task still completed\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("statistics unavailable"));
}

fn http(address: &str, path: &str, host: &str) -> (u16, String) {
    use std::io::Read;
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut text = String::new();
    stream.read_to_string(&mut text).unwrap();
    let (headers, body) = text.split_once("\r\n\r\n").unwrap();
    (
        headers.split_whitespace().nth(1).unwrap().parse().unwrap(),
        body.into(),
    )
}

fn start_stats_server(dir: &std::path::Path) -> (Cli, String) {
    use std::io::BufRead;
    let mut cli = Cli::spawn(dir, &["stats", "--port", "0"], &[]);
    let stdout = cli.0.as_mut().unwrap().stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        std::io::BufReader::new(stdout)
            .read_line(&mut line)
            .unwrap();
        tx.send(line).unwrap();
    });
    let line = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("stats server did not start");
    let address = line
        .trim()
        .strip_prefix("pp stats: http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_owned();
    (cli, address)
}

#[test]
fn stats_server_starts_without_models_and_rejects_foreign_hosts() {
    let dir = tempfile::tempdir().unwrap();
    let (_cli, address) = start_stats_server(dir.path());
    let address = address.as_str();
    let (status, body) = http(address, "/api/stats/overview?range=24h", address);
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["summary"]["requests"],
        0
    );
    assert_eq!(http(address, "/", address).0, 200);
    assert_eq!(
        http(address, "/api/stats/overview?range=bad", address).0,
        400
    );
    assert_eq!(http(address, "/api/stats/requests/999", address).0, 404);
    assert_eq!(
        http(
            address,
            "/api/stats/requests?range=all",
            "untrusted.example"
        )
        .0,
        403
    );
    let bad = Cli::spawn(dir.path(), &["-p", "invalid", "stats"], &[]).finish();
    assert!(!bad.status.success());
}

#[test]
fn stats_interrupted_streams_keep_partial_output_without_committing_session_entries() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            dir.path(),
            Some("partial statistics"),
            fixture_settings(),
            &response_models(&server, Some(protocol)),
        );
        let (request, _) = receive_at(
            &server,
            if protocol == "completion" {
                "/v1/chat/completions"
            } else {
                "/v1/responses"
            },
        );
        let mut stream = SseReply::new(request);
        if protocol == "completion" {
            stream.event(chat_chunk(
                json!({"role":"assistant","content":"partial model output"}),
                None,
            ));
        } else {
            stream.event(json!({"type":"response.output_text.delta","sequence_number":0,"item_id":"partial","output_index":0,"content_index":0,"delta":"partial model output","logprobs":[]}));
        }
        stream.finish();
        let output = cli.finish();
        assert!(!output.status.success());
        let records = rows(dir.path());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["status"], "failed");
        assert_eq!(records[0]["payload"]["partial"], true);
        assert_eq!(records[0]["payload"]["text"], "partial model output");
        assert!(records[0]["entry"].is_null());
        assert!(records[0]["ttft"].is_number());
        no_more(&server);
    }
}

#[test]
fn stats_truncated_chat_preserves_reported_usage() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let cli = Cli::start(&server, dir.path(), Some("truncated response"));
    let (request, _) = receive(&server);
    let mut stream = SseReply::new(request);
    stream.event(chat_chunk(
        json!({"role":"assistant","content":"truncated text"}),
        None,
    ));
    stream.event(chat_chunk(json!({}), Some("length")));
    stream.event(json!({"id":"usage","object":"chat.completion.chunk","created":1,"model":"fixture-model","choices":[],"usage":{"prompt_tokens":120,"completion_tokens":15,"total_tokens":135,"prompt_tokens_details":{"cached_tokens":100}}}));
    stream.frame("[DONE]");
    stream.finish();
    assert!(!cli.finish().status.success());
    let records = rows(dir.path());
    assert_eq!(records[0]["status"], "failed");
    assert_eq!(records[0]["input"], 120);
    assert_eq!(records[0]["output"], 15);
    assert_eq!(records[0]["cache"], 100);
    assert_eq!(records[0]["payload"]["native"]["finish_reason"], "length");
    assert!(records[0]["entry"].is_null());
}
