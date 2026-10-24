use super::*;
use rusqlite::Connection;

pub(super) fn database(home: &std::path::Path) -> Connection {
    Connection::open(home.join(".pp/stats/stats.db")).unwrap()
}
fn rows(home: &std::path::Path) -> Vec<Value> {
    database(home).prepare("SELECT r.id,r.status,r.input_tokens,r.output_tokens,r.cached_tokens,r.ttft_ms,r.purpose,r.entry_id,r.session_file,p.output_json,p.session_entry_json,r.error_message,r.cache_write_tokens FROM requests r JOIN request_payloads p ON p.request_id=r.id ORDER BY r.id").unwrap().query_map([],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"status":r.get::<_,String>(1)?,"input":r.get::<_,Option<i64>>(2)?,"output":r.get::<_,Option<i64>>(3)?,"cache":r.get::<_,Option<i64>>(4)?,"ttft":r.get::<_,Option<f64>>(5)?,"purpose":r.get::<_,String>(6)?,"entry_id":r.get::<_,Option<String>>(7)?,"session_file":r.get::<_,Option<String>>(8)?,"payload":serde_json::from_str::<Value>(&r.get::<_,String>(9)?).unwrap(),"entry":r.get::<_,Option<String>>(10)?.map(|s|serde_json::from_str::<Value>(&s).unwrap()),"error":r.get::<_,Option<String>>(11)?,"cache_write":r.get::<_,Option<i64>>(12)?}))).unwrap().collect::<Result<_,_>>().unwrap()
}

fn gc_counts(output: &Output) -> Vec<Vec<u64>> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            line.split(|character: char| !character.is_ascii_digit())
                .filter(|field| !field.is_empty())
                .map(|field| field.parse().unwrap())
                .collect()
        })
        .collect()
}

#[test]
fn gc_removes_expired_logs_and_sqlite_snapshots_without_models() {
    use chrono::{Days, Local, Utc};
    use std::{fs, os::unix::fs::symlink};

    let home = tempfile::tempdir().unwrap();
    let (_server, _address) = start_stats_server(home.path());
    let db = database(home.path());
    let cutoff = Utc::now().timestamp_millis() - 90 * 24 * 60 * 60 * 1000;
    for (id, timestamp) in [
        (1, cutoff - 60_000),
        (2, cutoff + 60_000),
        (3, cutoff + 91 * 24 * 60 * 60 * 1000),
    ] {
        db.execute("INSERT INTO requests(id,request_key,timestamp,model,provider,model_reference,api,project,cwd,purpose,duration_ms,status) VALUES (?1,?1,?2,'m','p','p/m','completion','project','/project','task',1,'ok')", rusqlite::params![id, timestamp]).unwrap();
        db.execute("INSERT INTO request_payloads(request_id,output_json,session_entry_json) VALUES (?1,?2,'[]')", rusqlite::params![id, if id == 1 { format!("\"{}\"", "x".repeat(1024 * 1024)) } else { "{}".to_owned() }]).unwrap();
    }
    let pages_before: i64 = db
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .unwrap();
    let logs = home.path().join(".pp/logs");
    let day = Local::now()
        .date_naive()
        .checked_sub_days(Days::new(90))
        .unwrap();
    let old = day.pred_opt().unwrap();
    let old_names = [
        format!("pp.{old}.1.log"),
        format!("debug.{old}.0001"),
        format!("prompt-debug.{old}T12-00-00-{}", "ab".repeat(16)),
    ];
    let kept_names = [
        format!("pp.{day}.1.log"),
        format!("debug.{day}.0001"),
        format!("prompt-debug.{day}T12-00-00-{}", "ab".repeat(16)),
        format!("pp.{old}.01.log"),
        "unrelated.log".to_owned(),
    ];
    for name in old_names.iter().chain(&kept_names) {
        fs::write(logs.join(name), b"fixture").unwrap();
    }
    let sentinel = home.path().join("sentinel");
    fs::write(&sentinel, b"keep").unwrap();
    let link = logs.join(format!("pp.{old}.2.log"));
    symlink(&sentinel, &link).unwrap();
    let directory = logs.join(format!("pp.{old}.3.log"));
    fs::create_dir(&directory).unwrap();
    let sessions = home.path().join(".pp/agent/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let session = sessions.join(format!("{old}T12-00-00-{}.jsonl", "ab".repeat(16)));
    fs::write(&session, b"saved session").unwrap();

    let output = Cli::spawn(home.path(), &["gc"], &[]).finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Three expired and three boundary logs, plus the server's current log.
    // Foreign names, symlinks, directories, and snapshots are not extra inventory.
    assert_eq!(gc_counts(&output), [vec![7, 3, 4], vec![3, 1, 2]]);
    for name in old_names {
        assert!(!logs.join(name).exists());
    }
    for name in kept_names {
        assert_eq!(fs::read(logs.join(name)).unwrap(), b"fixture");
    }
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(directory.is_dir());
    assert_eq!(fs::read(&sentinel).unwrap(), b"keep");
    assert_eq!(fs::read(&session).unwrap(), b"saved session");
    let remaining: Vec<i64> = db
        .prepare("SELECT id FROM requests ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(remaining, [2, 3]);
    let payload_ids: Vec<i64> = db
        .prepare("SELECT request_id FROM request_payloads ORDER BY request_id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(payload_ids, [2, 3]);
    let pages_after: i64 = db
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .unwrap();
    assert!(
        pages_after < pages_before,
        "VACUUM must reclaim expired payload pages"
    );
    assert_eq!(
        db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    let second = Cli::spawn(home.path(), &["gc"], &[]).finish();
    assert!(second.status.success());
    assert_eq!(gc_counts(&second), [vec![4, 0, 4], vec![2, 0, 2]]);
}

#[test]
fn gc_empty_home_does_not_create_storage_and_rejects_symlink_database() {
    use std::{fs, os::unix::fs::symlink};
    let home = tempfile::tempdir().unwrap();
    let output = Cli::spawn(home.path(), &["gc"], &[]).finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(gc_counts(&output), [vec![0, 0, 0], vec![0, 0, 0]]);
    assert!(!home.path().join(".pp").exists());
    let stats = home.path().join(".pp/stats");
    fs::create_dir_all(&stats).unwrap();
    let sentinel = home.path().join("sentinel");
    fs::write(&sentinel, b"keep").unwrap();
    symlink(&sentinel, stats.join("stats.db")).unwrap();
    let output = Cli::spawn(home.path(), &["gc"], &[]).finish();
    assert!(!output.status.success());
    assert_eq!(fs::read(&sentinel).unwrap(), b"keep");
}

#[test]
fn stats_chat_captures_usage_thinking_tools_and_real_session_entries() {
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
    stream.event(json!({"id":"usage","object":"chat.completion.chunk","created":1,"model":"fixture-model","choices":[],"usage":{"prompt_tokens":120,"completion_tokens":15,"total_tokens":135,"prompt_tokens_details":{"cached_tokens":100,"cache_write_tokens":10}}}));
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
    assert_eq!(last["cache_write"], 10);
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
    let (status, body) = http(&address, "/api/stats/errors?range=all", &address);
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["summary"]["requests"],
        0
    );
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
    assert!((detail["request"]["cost"].as_f64().unwrap() - 0.00037).abs() < 1e-10);
}

#[test]
fn stats_responses_captures_real_usage_and_keeps_unknown_cache_unknown() {
    for (cached, written) in [
        (Some(40), Some(5)),
        (Some(40), Some(0)),
        (Some(40), None),
        (None, None),
    ] {
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
            if let Some(written) = written {
                body["usage"]["input_tokens_details"]["cache_write_tokens"] = json!(written);
            }
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
        assert_eq!(records[0]["cache_write"], json!(written));
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
            // A zero rate-limit budget keeps this 429 from retrying, so the
            // single stats row stays one failed request.
            "{\n\"default_model\": \"fixture/primary\",\n\"rate_limit_retry_budget_seconds\": 0,\n}\n",
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
    stream.event(json!({"id":"usage","object":"chat.completion.chunk","created":1,"model":"fixture-model","choices":[],"usage":{"prompt_tokens":120,"completion_tokens":15,"total_tokens":135,"prompt_tokens_details":{"cached_tokens":100,"cache_write_tokens":10}}}));
    stream.frame("[DONE]");
    stream.finish();
    assert!(!cli.finish().status.success());
    let records = rows(dir.path());
    assert_eq!(records[0]["status"], "failed");
    assert_eq!(records[0]["input"], 120);
    assert_eq!(records[0]["output"], 15);
    assert_eq!(records[0]["cache"], 100);
    assert_eq!(records[0]["cache_write"], 10);
    assert_eq!(records[0]["payload"]["native"]["finish_reason"], "length");
    assert!(records[0]["entry"].is_null());
}

#[test]
fn stats_cache_write_extensions_preserve_unknown_zero_and_failed_usage() {
    for protocol in ["completion", "response"] {
        for (extension, expected) in [
            (json!({"cache_write_tokens":0}), Some(0)),
            (json!({"cache_write_tokens":5}), Some(5)),
            (json!({"cache_creation_input_tokens":5}), Some(5)),
            (json!({}), None),
            (json!({"cache_write_tokens":-1}), None),
            (json!({"cache_write_tokens":"5"}), None),
        ] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let cli = Cli::start_with(
                dir.path(),
                Some("cache write failure"),
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
                    json!({"role":"assistant","content":"truncated"}),
                    None,
                ));
                stream.event(chat_chunk(json!({}), Some("length")));
                let mut usage = json!({"prompt_tokens":50,"completion_tokens":10,"total_tokens":60,"prompt_tokens_details":{"cached_tokens":40}});
                usage
                    .as_object_mut()
                    .unwrap()
                    .extend(extension.as_object().unwrap().clone());
                stream.event(json!({"id":"usage","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[],"usage":usage}));
                stream.frame("[DONE]");
            } else {
                let mut body = response_body(json!([]));
                body["status"] = json!("incomplete");
                body["incomplete_details"] = json!({"reason":"max_output_tokens"});
                let mut usage = json!({"input_tokens":50,"output_tokens":10,"total_tokens":60,"input_tokens_details":{"cached_tokens":40},"output_tokens_details":{"reasoning_tokens":0}});
                usage
                    .as_object_mut()
                    .unwrap()
                    .extend(extension.as_object().unwrap().clone());
                body["usage"] = usage;
                stream.event(
                    json!({"type":"response.incomplete","sequence_number":0,"response":body}),
                );
            }
            stream.finish();
            assert!(!cli.finish().status.success());
            let records = rows(dir.path());
            assert_eq!(records.len(), 1);
            assert_eq!(records[0]["status"], "failed");
            assert_eq!(records[0]["input"], 50);
            assert_eq!(records[0]["cache_write"], json!(expected));
            assert!(records[0]["entry"].is_null());
        }
    }
}
