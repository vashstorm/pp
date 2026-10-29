use super::*;
use async_openai::types::chat::ChatCompletionRequestAssistantMessageContent;
use serde_json::{Value, json};
use std::io::Write;

#[test]
fn read_http_redirects_without_provider_auth_and_redacts_errors() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}/first", server.server_addr());
    let worker =
        std::thread::spawn(move || {
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert!(!request.headers().iter().any(|h| matches!(
                h.field.as_str().as_str().to_ascii_lowercase().as_str(),
                "authorization" | "openai-organization" | "openai-project" | "cookie"
            )));
            request
                .respond(
                    tiny_http::Response::empty(302)
                        .with_header(tiny_http::Header::from_bytes("Location", "/final").unwrap()),
                )
                .unwrap();
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(request.url(), "/final");
            request
                .respond(tiny_http::Response::from_string("web body").with_header(
                    tiny_http::Header::from_bytes("Content-Type", "text/plain").unwrap(),
                ))
                .unwrap();
        });
    let mut client = test_client("http://127.0.0.1:1/v1");
    let response = client
        .fetch_read_url(&url, &CancellationToken::new())
        .unwrap();
    assert_eq!(response.bytes, b"web body");
    assert!(response.final_url.ends_with("/final"));
    worker.join().unwrap();
    for url in [
        "http://user:secret@127.0.0.1/",
        "ftp://example.com/",
        "bad-url-secret",
    ] {
        let message = client
            .fetch_read_url(url, &CancellationToken::new())
            .err()
            .unwrap()
            .to_string();
        assert!(!message.contains("secret") && !message.contains("private-key"));
    }
}

#[test]
fn read_http_limits_redirects_and_response_body_without_retries() {
    for oversize in [false, true] {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/loop", server.server_addr());
        let worker = std::thread::spawn(move || {
            for _ in 0..if oversize { 1 } else { 6 } {
                let request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                if oversize {
                    let _ = request.respond(tiny_http::Response::from_data(vec![
                        b'x';
                        crate::tools::read::MAX_INPUT_BYTES
                            + 1
                    ]));
                } else {
                    request
                        .respond(tiny_http::Response::empty(302).with_header(
                            tiny_http::Header::from_bytes("Location", "/loop").unwrap(),
                        ))
                        .unwrap();
                }
            }
            assert!(
                server
                    .recv_timeout(Duration::from_millis(100))
                    .unwrap()
                    .is_none()
            );
        });
        let error = test_client("http://127.0.0.1:1/v1")
            .fetch_read_url(&url, &CancellationToken::new())
            .err()
            .unwrap();
        if oversize {
            assert!(error.to_string().contains("20 MiB"));
        }
        worker.join().unwrap();
    }
}

#[test]
fn read_http_gzip_limit_counts_decompressed_bytes() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}/compressed", server.server_addr());
    let worker = std::thread::spawn(move || {
        let request = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let block = [b'x'; 8192];
        for _ in 0..(crate::tools::read::MAX_INPUT_BYTES / block.len() + 1) {
            encoder.write_all(&block).unwrap();
        }
        let compressed = encoder.finish().unwrap();
        assert!(compressed.len() < crate::tools::read::MAX_INPUT_BYTES);
        let _ = request.respond(
            tiny_http::Response::from_data(compressed)
                .with_header(tiny_http::Header::from_bytes("Content-Encoding", "gzip").unwrap()),
        );
    });
    let error = test_client("http://127.0.0.1:1/v1")
        .fetch_read_url(&url, &CancellationToken::new())
        .err()
        .unwrap();
    assert!(error.to_string().contains("20 MiB"), "{error}");
    worker.join().unwrap();
}

#[test]
fn read_http_status_timeout_and_cancel_leave_no_background_fetch() {
    for mode in ["status", "timeout", "cancel"] {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/private-query", server.server_addr());
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let (done, release) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            if mode == "status" {
                request
                    .respond(tiny_http::Response::from_string("secret-body").with_status_code(429))
                    .unwrap();
            } else {
                let mut writer = request.into_writer();
                if mode == "cancel" {
                    writer
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n",
                        )
                        .unwrap();
                    writer.flush().unwrap();
                    worker_cancel.cancel();
                }
                release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            assert!(
                server
                    .recv_timeout(Duration::from_millis(100))
                    .unwrap()
                    .is_none()
            );
        });
        let mut client = test_client("http://127.0.0.1:1/v1");
        if mode == "timeout" {
            client.read_http = Some(
                reqwest::Client::builder()
                    .timeout(Duration::from_millis(100))
                    .build()
                    .unwrap(),
            );
        }
        let started = Instant::now();
        let error = client.fetch_read_url(&url, &cancel).err().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        if mode == "cancel" {
            assert!(error.is::<Cancelled>());
        }
        if mode == "status" {
            assert!(error.to_string().contains("429"));
        }
        assert!(
            !error.to_string().contains("secret-body")
                && !error.to_string().contains("private-query")
        );
        let _ = done.send(());
        worker.join().unwrap();
    }
}

fn decode(message: Value, finish: &str) -> Result<CompletionTurn> {
    into_completion_turn(serde_json::from_value(json!({
        "id":"chatcmpl-fixture", "object":"chat.completion", "created":0, "model":"fixture",
        "choices":[{"index":0,"message":message,"finish_reason":finish}]
    }))?)
}

#[test]
fn auth_client_reloads_credentials_headers_and_observes_logout() {
    use crate::auth::{Credential, Provider, Store};
    use async_openai::config::Config as _;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.yml");
    let mut store = Store::default();
    let mut credential = Credential::api_key("first-github-token".into());
    credential.api_endpoint = Some("https://api.business.githubcopilot.com".into());
    store
        .credentials
        .insert(Provider::GithubCopilot, credential);
    store.save(&path).unwrap();
    let mut client = test_client("http://127.0.0.1:1");
    client.auth = Some(crate::auth::Config {
        provider: Provider::GithubCopilot,
        path: path.clone(),
        generation: String::new(),
    });
    let cancellation = CancellationToken::new();
    client.prepare_auth(&cancellation, "user", true).unwrap();
    let sdk = client.client.config();
    assert_eq!(sdk.api_base(), "https://api.business.githubcopilot.com");
    assert_eq!(sdk.headers()["authorization"], "Bearer first-github-token");
    assert_eq!(sdk.headers()["x-initiator"], "user");
    assert_eq!(sdk.headers()["copilot-vision-request"], "true");
    store
        .credentials
        .get_mut(&Provider::GithubCopilot)
        .unwrap()
        .access_token = "second-github-token".into();
    store.save(&path).unwrap();
    client.prepare_auth(&cancellation, "agent", false).unwrap();
    let headers = client.client.config().headers();
    assert_eq!(headers["authorization"], "Bearer second-github-token");
    assert_eq!(headers["x-initiator"], "agent");
    assert!(!headers.contains_key("copilot-vision-request"));
    store
        .credentials
        .get_mut(&Provider::GithubCopilot)
        .unwrap()
        .generation = "new-account".into();
    store.save(&path).unwrap();
    let error = client
        .prepare_auth(&cancellation, "user", false)
        .err()
        .unwrap();
    assert!(error.to_string().contains("Authorization changed"));
    client.auth.as_mut().unwrap().generation = "new-account".into();
    client.prepare_auth(&cancellation, "user", false).unwrap();
    store.credentials.clear();
    store.save(&path).unwrap();
    assert!(client.prepare_auth(&cancellation, "user", false).is_err());
}

fn test_client(base_url: &str) -> OpenAiClient {
    test_client_with_rate_limit_budget(base_url, 0)
}

fn test_client_with_rate_limit_budget(base_url: &str, budget_seconds: u64) -> OpenAiClient {
    OpenAiClient::new(&Config {
        auth: None,
        api_key: "private-key".into(),
        model: "fixture".into(),
        protocol: crate::models::Protocol::Completion,
        thinking: crate::models::ModelThinking::default(),
        base_url: base_url.parse().unwrap(),
        context_window: None,
        settings: crate::config::Settings {
            task: Default::default(),
            grep: Default::default(),
            read: Default::default(),
            tools: Default::default(),
            debug: false,
            notification_show_countdown: true,
            notification_timeout_seconds: 10,
            mcp_servers: Default::default(),
            skill_dirs: Vec::new(),
            skill_sources: Vec::new(),
            disabled_skills: Vec::new(),
            think_level: crate::config::ThinkLevel::None,
            edit: crate::config::EditMode::Replace,
            default_model: None,
            favorite_models: Vec::new(),
            request_timeout_seconds: 120,
            rate_limit_retry_budget_seconds: budget_seconds,
            read_max_bytes: 65_536,
            read_max_lines: 2_000,
            bash_timeout_seconds: 120,
            auto_compact: true,
            auto_compact_threshold_percent: 80,
        },
    })
    .unwrap()
}

fn chat_chunk(delta: Value, finish: Option<&str>) -> Value {
    json!({
        "id":"chatcmpl-fixture", "object":"chat.completion.chunk",
        "created":1, "model":"fixture-model",
        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
    })
}

fn sse_writer(request: tiny_http::Request) -> Box<dyn Write + Send> {
    let mut writer = request.into_writer();
    writer
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .unwrap();
    writer.flush().unwrap();
    writer
}

fn sse_frame(writer: &mut Box<dyn Write + Send>, data: &str) {
    let frame = format!("data: {data}\n\n");
    write!(writer, "{:x}\r\n{}\r\n", frame.len(), frame).unwrap();
    writer.flush().unwrap();
}

fn sse_end(mut writer: Box<dyn Write + Send>) {
    // The peer may already be gone after cancellation; closing is best effort.
    let _ = writer.write_all(b"0\r\n\r\n");
    let _ = writer.flush();
}

#[test]
fn http_errors_hide_body_and_redirects_are_not_followed() {
    for status in [302, 401, 429, 500] {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", server.server_addr());
        let mut client = test_client(&base_url);
        let token = CancellationToken::new();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let request = server
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                request
                    .respond(
                        tiny_http::Response::from_string("private-body")
                            .with_status_code(status)
                            .with_header(
                                tiny_http::Header::from_bytes("Location", base_url.as_str())
                                    .unwrap(),
                            ),
                    )
                    .unwrap();
            });
            let mut sent = 0;
            let mut finished = None;
            let error = client
                .complete(
                    &CreateChatCompletionRequest::default(),
                    &token,
                    &mut || {
                        sent += 1;
                        Ok(())
                    },
                    &mut |event| {
                        if let AgentEvent::ModelRequestFinished { usage, success, .. } = event {
                            finished = Some((usage, success));
                        }
                    },
                )
                .err()
                .unwrap()
                .to_string();
            assert_eq!(finished, Some((None, false)));
            assert_eq!(sent, 1);
            assert!(error.contains(&status.to_string()));
            assert!(!error.contains("private-body") && !error.contains("private-key"));
        });
        assert!(
            server
                .recv_timeout(Duration::from_millis(50))
                .unwrap()
                .is_none()
        );
    }
}

fn api_error(status: u16, code: Option<&str>) -> OpenAIError {
    OpenAIError::ApiError(ApiErrorResponse {
        status_code: reqwest::StatusCode::from_u16(status).unwrap(),
        api_error: ApiError {
            message: String::new(),
            r#type: None,
            param: None,
            code: code.map(str::to_owned),
            misalignment: None,
        },
    })
}

#[test]
fn request_retry_wait_honors_header_exponential_fallback_and_budget() {
    let budget = Duration::from_secs(300);
    let no_header = api_error(429, None);
    assert_eq!(
        request_retry_wait(&no_header, Duration::ZERO, budget, 0),
        Some(Duration::from_secs(1))
    );
    assert_eq!(
        request_retry_wait(&no_header, Duration::ZERO, budget, 1),
        Some(Duration::from_secs(2))
    );
    assert_eq!(
        request_retry_wait(&no_header, Duration::ZERO, budget, 3),
        Some(Duration::from_secs(8))
    );
    let header = api_error(429, Some("pp-retry-after:7"));
    assert_eq!(
        request_retry_wait(&header, Duration::ZERO, budget, 0),
        Some(Duration::from_secs(7))
    );
    // Zero seconds means the provider allows an immediate retry.
    assert_eq!(
        request_retry_wait(
            &api_error(429, Some("pp-retry-after:0")),
            Duration::ZERO,
            budget,
            5
        ),
        Some(Duration::ZERO)
    );
    // Waits are clipped to the remaining budget.
    assert_eq!(
        request_retry_wait(&header, Duration::from_secs(296), budget, 0),
        Some(Duration::from_secs(4))
    );
    // 预算耗尽、关闭重试、达到次数上限及永久错误均停止重试。
    assert_eq!(request_retry_wait(&header, budget, budget, 0), None);
    assert_eq!(
        request_retry_wait(&header, Duration::ZERO, Duration::ZERO, 0),
        None
    );
    assert_eq!(
        request_retry_wait(&no_header, Duration::ZERO, budget, 9),
        None
    );
    assert_eq!(
        request_retry_wait(&api_error(400, None), Duration::ZERO, budget, 0),
        None
    );
    // Only the middleware's own marker counts as a Retry-After value.
    assert_eq!(
        request_retry_wait(&api_error(429, Some("7")), Duration::ZERO, budget, 0),
        Some(Duration::from_secs(1))
    );
}

#[test]
fn request_retry_classifies_transient_statuses_and_rejects_permanent_errors() {
    use crate::events::RequestRetryReason;
    for status in [408, 409, 429, 500, 502, 503, 504, 599] {
        let error = api_error(status, Some("pp-retry-after:3"));
        assert!(request_retry_reason(&error).is_some(), "{status}");
        assert_eq!(
            request_retry_wait(&error, Duration::ZERO, Duration::from_secs(10), 0),
            Some(Duration::from_secs(3)),
            "{status}"
        );
        assert_eq!(
            request_retry_wait(&error, Duration::ZERO, Duration::ZERO, 0),
            None
        );
    }
    assert_eq!(
        request_retry_reason(&api_error(503, None)),
        Some(RequestRetryReason::HttpStatus(503))
    );
    for status in [302, 400, 401, 403, 404, 422] {
        assert!(
            request_retry_reason(&api_error(status, None)).is_none(),
            "{status}"
        );
    }
    assert!(
        request_retry_reason(&OpenAIError::InvalidArgument("private argument".into())).is_none()
    );
    let invalid_json = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
    assert!(
        request_retry_reason(&OpenAIError::JSONDeserialize(
            invalid_json,
            "private body".into()
        ))
        .is_none()
    );
}

#[test]
fn request_retry_attempt_cap_and_cancellation_stop_reopening() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for cancel in [false, true] {
        let token = CancellationToken::new();
        let mut attempts = 1;
        let mut events = 0;
        let error = runtime
            .block_on(open_with_request_retry(
                &token,
                Duration::from_secs(30),
                "fixture",
                &mut |event| {
                    if let AgentEvent::RequestRetrying { .. } = event {
                        events += 1;
                        if cancel {
                            token.cancel();
                        }
                    }
                },
                Err::<(), _>(api_error(503, Some("pp-retry-after:0"))),
                || {
                    attempts += 1;
                    std::future::ready(Err(api_error(503, Some("pp-retry-after:0"))))
                },
            ))
            .unwrap_err();
        if cancel {
            assert_eq!(attempts, 1);
            assert_eq!(events, 1);
            assert!(error.downcast_ref::<Cancelled>().is_some());
        } else {
            assert_eq!(attempts, 10);
            assert_eq!(events, 9);
            assert!(error.to_string().contains("gave up after 9 retries"));
        }
    }
}

#[test]
fn opening_error_notes_exhausted_budget_only_after_retries() {
    assert_eq!(
        opening_error(api_error(429, None), "Chat Completions", Duration::ZERO, 0).to_string(),
        "Chat Completions HTTP status 429 Too Many Requests"
    );
    let message = opening_error(
        api_error(429, None),
        "Chat Completions",
        Duration::from_secs(3),
        2,
    )
    .to_string();
    assert!(message.contains("HTTP status 429"));
    assert!(message.contains("gave up after 2 retries and 3s of backoff"));
    assert_eq!(
        opening_error(
            api_error(500, None),
            "Chat Completions",
            Duration::from_secs(3),
            2
        )
        .to_string(),
        "Chat Completions HTTP status 500 Internal Server Error (gave up after 2 retries and 3s of backoff)"
    );
}

#[test]
fn rate_limited_opening_retries_then_finishes_the_request() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let mut client =
        test_client_with_rate_limit_budget(&format!("http://{}", server.server_addr()), 30);
    let token = CancellationToken::new();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let first = server
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            first
                .respond(
                    tiny_http::Response::from_string("private-body fixture")
                        .with_status_code(429)
                        .with_header(tiny_http::Header::from_bytes("Retry-After", "0").unwrap()),
                )
                .unwrap();
            let second = server
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            let mut writer = sse_writer(second);
            sse_frame(
                &mut writer,
                &chat_chunk(json!({"role":"assistant","content":"recovered"}), None).to_string(),
            );
            sse_frame(
                &mut writer,
                &chat_chunk(json!({}), Some("stop")).to_string(),
            );
            sse_frame(&mut writer, "[DONE]");
            sse_end(writer);
        });
        let mut sent = 0;
        let mut rate_limited = Vec::new();
        let turn = client
            .complete(
                &CreateChatCompletionRequest::default(),
                &token,
                &mut || {
                    sent += 1;
                    Ok(())
                },
                &mut |event| {
                    if let AgentEvent::RequestRetrying {
                        attempt, wait_ms, ..
                    } = event
                    {
                        rate_limited.push((attempt, wait_ms));
                    }
                },
            )
            .unwrap();
        assert_eq!(sent, 1);
        assert_eq!(rate_limited, vec![(1, 0)]);
        assert_eq!(
            turn.message.content,
            Some(ChatCompletionRequestAssistantMessageContent::Text(
                "recovered".into()
            ))
        );
    });
    assert!(
        server
            .recv_timeout(Duration::from_millis(50))
            .unwrap()
            .is_none()
    );
}

#[test]
fn rate_limited_opening_gives_up_when_budget_is_spent() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    // A 1s budget without a Retry-After header: the first backoff of 1s
    // spends it entirely, so the second 429 fails the request.
    let mut client =
        test_client_with_rate_limit_budget(&format!("http://{}", server.server_addr()), 1);
    let token = CancellationToken::new();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for _ in 0..2 {
                let request = server
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                request
                    .respond(
                        tiny_http::Response::from_string("private-body fixture")
                            .with_status_code(429),
                    )
                    .unwrap();
            }
        });
        let mut sent = 0;
        let mut rate_limited = Vec::new();
        let mut finished = None;
        let error = client
            .complete(
                &CreateChatCompletionRequest::default(),
                &token,
                &mut || {
                    sent += 1;
                    Ok(())
                },
                &mut |event| match event {
                    AgentEvent::RequestRetrying {
                        attempt, wait_ms, ..
                    } => {
                        rate_limited.push((attempt, wait_ms));
                    }
                    AgentEvent::ModelRequestFinished { success, .. } => {
                        finished = Some(success);
                    }
                    _ => {}
                },
            )
            .err()
            .unwrap()
            .to_string();
        assert_eq!(sent, 1);
        assert_eq!(rate_limited, vec![(1, 1000)]);
        assert_eq!(finished, Some(false));
        assert!(error.contains("HTTP status 429"));
        assert!(error.contains("gave up after 1 retries and 1s of backoff"));
        assert!(!error.contains("private-body"));
    });
    assert!(
        server
            .recv_timeout(Duration::from_millis(50))
            .unwrap()
            .is_none()
    );
}

#[test]
fn cancelled_before_request_reports_cancelled_without_sending() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let mut client = test_client(&format!("http://{}", server.server_addr()));
    let token = CancellationToken::new();
    token.cancel();
    let mut sent = 0;
    let error = client
        .complete(
            &CreateChatCompletionRequest::default(),
            &token,
            &mut || {
                sent += 1;
                Ok(())
            },
            &mut |_| {},
        )
        .err()
        .unwrap();
    assert_eq!(sent, 1);
    assert!(error.downcast_ref::<Cancelled>().is_some());
    assert!(
        server
            .recv_timeout(Duration::from_millis(50))
            .unwrap()
            .is_none()
    );
}

#[test]
fn sent_callback_error_takes_precedence_over_cancellation() {
    let mut client = test_client("http://127.0.0.1:1");
    let token = CancellationToken::new();
    token.cancel();
    let mut sent = 0;
    let error = client
        .complete(
            &CreateChatCompletionRequest::default(),
            &token,
            &mut || {
                sent += 1;
                bail!("persisting session failed")
            },
            &mut |_| {},
        )
        .err()
        .unwrap();
    assert_eq!(sent, 1);
    assert_eq!(error.to_string(), "persisting session failed");
    assert!(error.downcast_ref::<Cancelled>().is_none());
}

#[test]
fn cancelled_stream_is_dropped_and_next_request_rebuilds_runtime() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let mut client = test_client(&format!("http://{}", server.server_addr()));
    let (release_first, first_released) = std::sync::mpsc::channel::<()>();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            // First request: a single partial delta, then the response is held open.
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let mut writer = sse_writer(request);
            sse_frame(
                &mut writer,
                &chat_chunk(json!({"role":"assistant","content":"partial"}), None).to_string(),
            );
            first_released.recv_timeout(Duration::from_secs(5)).unwrap();
            sse_end(writer);
            // Second request: a fresh client must reconnect and finish normally.
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let mut writer = sse_writer(request);
            sse_frame(
                &mut writer,
                &chat_chunk(json!({"role":"assistant","content":"recovered"}), None).to_string(),
            );
            sse_frame(
                &mut writer,
                &chat_chunk(json!({}), Some("stop")).to_string(),
            );
            sse_frame(&mut writer, "[DONE]");
            sse_end(writer);
        });
        let token = CancellationToken::new();
        let mut sent = 0;
        let mut events = 0;
        let mut finished = None;
        let error = client
            .complete(
                &CreateChatCompletionRequest::default(),
                &token,
                &mut || {
                    sent += 1;
                    Ok(())
                },
                &mut |event| match event {
                    // Cancel from the first real stream delta, not from the
                    // request lifecycle events that bracket the stream.
                    AgentEvent::AssistantDelta { .. } => {
                        events += 1;
                        token.cancel();
                    }
                    AgentEvent::ModelRequestFinished { usage, success, .. } => {
                        finished = Some((usage, success));
                    }
                    _ => {}
                },
            )
            .err()
            .unwrap();
        assert!(events > 0);
        assert_eq!(sent, 1);
        assert_eq!(finished, Some((None, false)));
        assert!(error.downcast_ref::<Cancelled>().is_some());
        // Retire the cancelled stream's runtime; the next request rebuilds it.
        client.cancel_pending_io();
        assert!(client.runtime.is_none());
        release_first.send(()).unwrap();
        let token = CancellationToken::new();
        let mut sent = 0;
        let mut finished = None;
        let turn = client
            .complete(
                &CreateChatCompletionRequest::default(),
                &token,
                &mut || {
                    sent += 1;
                    Ok(())
                },
                &mut |event| {
                    if let AgentEvent::ModelRequestFinished { usage, success, .. } = event {
                        finished = Some((usage, success));
                    }
                },
            )
            .unwrap();
        assert_eq!(finished, Some((None, true)));
        assert_eq!(sent, 1);
        assert!(client.runtime.is_some());
        assert_eq!(
            turn.message.content,
            Some(ChatCompletionRequestAssistantMessageContent::Text(
                "recovered".into()
            ))
        );
        assert!(turn.message.tool_calls.is_none());
        assert_eq!(turn.usage, None);
        assert!(!turn.refusal);
    });
}

#[test]
fn refuses_truncated_or_invalid_batches() {
    let call = json!({"id":"x","type":"function","function":{"name":"write","arguments":"{}"}});
    for finish in ["length", "content_filter", "unknown"] {
        assert!(decode(json!({"role":"assistant","tool_calls":[call]}), finish).is_err());
    }
    assert!(
        decode(
            json!({"role":"assistant","tool_calls":[call,call]}),
            "tool_calls"
        )
        .is_err()
    );
    assert!(decode(json!({"role":"user","content":"wrong"}), "stop").is_err());
    assert!(decode(json!({"role":"assistant","content":null}), "stop").is_err());
    assert!(
        decode(
            json!({"role":"assistant","tool_calls":[call, {
                "id":"custom", "type":"custom", "custom_tool":{"name":"write","input":"{}"}
            }]}),
            "tool_calls"
        )
        .is_err()
    );
}

#[test]
fn accepts_refusal_and_nullable_calls() {
    for message in [
        json!({"role":"assistant","refusal":"Cannot help"}),
        json!({"role":"assistant","content":null,"tool_calls":null,"refusal":"Cannot help"}),
    ] {
        let turn = decode(message, "stop").unwrap();
        assert_eq!(
            turn.message.content,
            Some(ChatCompletionRequestAssistantMessageContent::Text(
                "Cannot help".into()
            ))
        );
        assert!(turn.message.tool_calls.is_none());
        assert!(turn.refusal);
        assert_eq!(turn.usage, None);
    }
}

#[test]
fn context_chat_refusal_flag_requires_no_normal_content() {
    let refusal_only = decode(json!({"role":"assistant","refusal":"no"}), "stop").unwrap();
    assert!(refusal_only.refusal);
    let mixed = decode(
        json!({"role":"assistant","content":"answer","refusal":"no"}),
        "stop",
    )
    .unwrap();
    assert!(!mixed.refusal);
    let blank_content = decode(
        json!({"role":"assistant","content":"  ","refusal":"no"}),
        "stop",
    )
    .unwrap();
    assert!(blank_content.refusal);
    let text_only = decode(json!({"role":"assistant","content":"answer"}), "stop").unwrap();
    assert!(!text_only.refusal);
}

#[test]
fn context_chat_cached_breakdown_preserves_unknown_and_zero() {
    for (details, expected) in [
        (None, None),
        (Some(json!({"cached_tokens":0})), Some(0)),
        (Some(json!({"cached_tokens":40})), Some(40)),
    ] {
        let mut response = json!({
            "id":"fixture", "object":"chat.completion", "created":1, "model":"fixture",
            "choices":[{"index":0,"message":{"role":"assistant","content":"answer"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":12000,"completion_tokens":300,"total_tokens":12300}
        });
        if let Some(details) = details {
            response["usage"]["prompt_tokens_details"] = details;
        }
        let turn = into_completion_turn(serde_json::from_value(response).unwrap()).unwrap();
        assert_eq!(
            turn.usage,
            Some(TokenUsage {
                input_tokens: 12000,
                output_tokens: 300,
                cache_write_tokens: None,
                cached_tokens: expected,
            })
        );
    }
}

#[test]
fn context_chat_usage_only_chunk_after_finish_returns_usage() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let mut client = test_client(&format!("http://{}", server.server_addr()));
    std::thread::scope(|scope| {
        scope.spawn(move || {
                // First request: normal chunks, then a choices-empty usage-only chunk.
                let request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let mut writer = sse_writer(request);
                sse_frame(
                    &mut writer,
                    &chat_chunk(json!({"role":"assistant","content":"answer"}), None).to_string(),
                );
                sse_frame(&mut writer, &chat_chunk(json!({}), Some("stop")).to_string());
                sse_frame(
                    &mut writer,
                    r#"{"id":"chatcmpl-fixture","object":"chat.completion.chunk","created":2,"model":"fixture-model","choices":[],"usage":{"prompt_tokens":12000,"completion_tokens":300,"total_tokens":12300}}"#,
                );
                sse_frame(&mut writer, "[DONE]");
                sse_end(writer);
                // Second request: no usage at all; the answer still succeeds.
                let request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let mut writer = sse_writer(request);
                sse_frame(
                    &mut writer,
                    &chat_chunk(json!({"role":"assistant","content":"again"}), Some("stop"))
                        .to_string(),
                );
                sse_frame(&mut writer, "[DONE]");
                sse_end(writer);
            });
        let token = CancellationToken::new();
        let mut finished = None;
        let turn = client
            .complete(
                &CreateChatCompletionRequest::default(),
                &token,
                &mut || Ok(()),
                &mut |event| {
                    if let AgentEvent::ModelRequestFinished { usage, success, .. } = event {
                        finished = Some((usage, success));
                    }
                },
            )
            .unwrap();
        assert_eq!(
            turn.usage,
            Some(TokenUsage {
                input_tokens: 12000,
                output_tokens: 300,
                cache_write_tokens: None,
                cached_tokens: None,
            })
        );
        assert_eq!(finished, Some((turn.usage, true)));
        assert_eq!(
            turn.message.content,
            Some(ChatCompletionRequestAssistantMessageContent::Text(
                "answer".into()
            ))
        );
        assert!(!turn.refusal);
        let token = CancellationToken::new();
        let mut finished = None;
        let turn = client
            .complete(
                &CreateChatCompletionRequest::default(),
                &token,
                &mut || Ok(()),
                &mut |event| {
                    if let AgentEvent::ModelRequestFinished { usage, success, .. } = event {
                        finished = Some((usage, success));
                    }
                },
            )
            .unwrap();
        assert_eq!(turn.usage, None);
        assert_eq!(finished, Some((None, true)));
        assert_eq!(
            turn.message.content,
            Some(ChatCompletionRequestAssistantMessageContent::Text(
                "again".into()
            ))
        );
    });
}

#[test]
fn context_responses_usage_extracted_and_missing_usage_is_none() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let mut client = test_client(&format!("http://{}", server.server_addr()));
    let completed = |usage: Option<Value>| {
        let mut response = json!({
            "id":"fixture", "object":"response", "created_at":0, "model":"fixture",
            "status":"completed",
            "output":[{"type":"message","id":"m","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"answer","annotations":[]}]}]
        });
        if let Some(usage) = usage {
            response["usage"] = usage;
        }
        json!({"type":"response.completed","sequence_number":5,"response":response}).to_string()
    };
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let mut writer = sse_writer(request);
            sse_frame(
                &mut writer,
                &completed(Some(json!({
                    "input_tokens":12000, "output_tokens":300, "total_tokens":12300,
                    "input_tokens_details":{"cached_tokens":40},
                    "output_tokens_details":{"reasoning_tokens":50}
                }))),
            );
            sse_end(writer);
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let mut writer = sse_writer(request);
            sse_frame(&mut writer, &completed(None));
            sse_end(writer);
        });
        let token = CancellationToken::new();
        let mut finished = None;
        let turn = client
            .respond(
                &CreateResponse::default(),
                &token,
                &mut || Ok(()),
                &mut |event| {
                    if let AgentEvent::ModelRequestFinished { usage, success, .. } = event {
                        finished = Some((usage, success));
                    }
                },
            )
            .unwrap();
        assert_eq!(turn.text, "answer");
        assert!(!turn.has_calls);
        assert!(!turn.refusal);
        assert_eq!(
            turn.usage,
            Some(TokenUsage {
                input_tokens: 12000,
                output_tokens: 300,
                cache_write_tokens: None,
                cached_tokens: Some(40),
            })
        );
        assert_eq!(finished, Some((turn.usage, true)));
        let token = CancellationToken::new();
        let mut finished = None;
        let turn = client
            .respond(
                &CreateResponse::default(),
                &token,
                &mut || Ok(()),
                &mut |event| {
                    if let AgentEvent::ModelRequestFinished { usage, success, .. } = event {
                        finished = Some((usage, success));
                    }
                },
            )
            .unwrap();
        assert_eq!(turn.usage, None);
        assert_eq!(finished, Some((None, true)));
    });
}
