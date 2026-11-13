use super::*;
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
};

#[test]
fn configured_request_timeout_stops_unresponsive_provider() {
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
            Some("stall"),
            "{\n\"default_model\": \"fixture/primary\",\n\"request_timeout_seconds\": 1,\n\"rate_limit_retry_budget_seconds\": 0,\n}\n",
            &models,
        );
        // Receive the request and hold it without ever responding.
        let (_request, _) = if responses {
            receive_at(&server, "/v1/responses")
        } else {
            receive(&server)
        };
        let output = cli.finish();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("timed out"), "{error}");
        no_more(&server);
    }
}

#[test]
fn provider_errors_are_redacted_and_never_retried_on_either_protocol() {
    for responses in [false, true] {
        for status in [200u16, 400, 429, 500] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let models = if responses {
                response_models(&server, Some("response"))
            } else {
                fixture_models(&server)
            };
            let cli = Cli::start_with(
                dir.path(),
                Some("do not retry a failed request"),
                // 显式关闭重试，验证单次失败的脱敏与终态。
                "{\n\"default_model\": \"fixture/primary\",\n\"rate_limit_retry_budget_seconds\": 0,\n}\n",
                &models,
            );
            let (request, _) = if responses {
                receive_at(&server, "/v1/responses")
            } else {
                receive(&server)
            };
            let body = if status == 200 {
                "malformed JSON containing fixture-key".to_owned()
            } else {
                json!({
                    "error": {
                        "message": "upstream exposed fixture-key",
                        "type": "fixture-key",
                        "param": "fixture-key",
                        "code": "fixture-key"
                    }
                })
                .to_string()
            };
            request
                .respond(
                    Response::builder()
                        .status(status)
                        .body(Body::from(body))
                        .unwrap(),
                )
                .unwrap();
            let output = cli.finish();
            assert!(
                !output.status.success(),
                "responses={responses}, status={status}"
            );
            assert!(output.stdout.is_empty());
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(!error.is_empty());
            assert!(!error.contains("fixture-key"), "{error}");
            no_more(&server);
        }
    }
}

#[test]
fn rate_limited_opening_retries_then_finishes_on_either_protocol() {
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
            Some("retry once"),
            "{\n\"default_model\": \"fixture/primary\",\n\"rate_limit_retry_budget_seconds\": 30,\n}\n",
            &models,
        );
        let (first, first_body) = if responses {
            receive_at(&server, "/v1/responses")
        } else {
            receive(&server)
        };
        first
            .respond(
                Response::builder()
                    .status(429)
                    .header("Retry-After", "0")
                    .body(Body::from("private-error-body fixture-key"))
                    .unwrap(),
            )
            .unwrap();
        let (second, second_body) = if responses {
            receive_at(&server, "/v1/responses")
        } else {
            receive(&server)
        };
        // The retried request carries the same credentials.
        assert_eq!(authorization(&second), "Bearer fixture-key");
        assert_eq!(first_body, second_body);
        if responses {
            response_final(second, "recovered");
        } else {
            final_reply(second, "recovered");
        }
        let output = cli.finish();
        assert!(output.status.success(), "responses={responses}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "recovered\n");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("rate limited; retrying in 0s (attempt 1)"),
            "responses={responses}, {error}"
        );
        assert!(!error.contains("fixture-key"), "{error}");
        let contexts = stats::contexts(dir.path());
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0]["request"], first_body);
        no_more(&server);
    }
}

#[test]
fn request_retry_recovers_transient_http_failures_on_either_protocol() {
    for responses in [false, true] {
        for status in [408u16, 409, 500, 502, 503, 504] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let models = if responses {
                response_models(&server, Some("response"))
            } else {
                fixture_models(&server)
            };
            let cli = Cli::start_with(
                dir.path(),
                Some("recover request"),
                "default_model: fixture/primary\n",
                &models,
            );
            let path = if responses {
                "/v1/responses"
            } else {
                "/v1/chat/completions"
            };
            let (first, first_body) = receive_at(&server, path);
            first
                .respond(
                    Response::builder()
                        .status(status)
                        .header("Retry-After", "0")
                        .body(Body::from("private-error-body fixture-key"))
                        .unwrap(),
                )
                .unwrap();
            let (second, second_body) = receive_at(&server, path);
            assert_eq!(first_body, second_body);
            assert_eq!(authorization(&second), "Bearer fixture-key");
            if responses {
                response_final(second, "recovered");
            } else {
                final_reply(second, "recovered");
            }
            let output = cli.finish();
            assert!(
                output.status.success(),
                "responses={responses}, status={status}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"recovered\n");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains(&format!("HTTP status {status}; retrying in 0s (attempt 1)")),
                "{stderr}"
            );
            assert!(!stderr.contains("fixture-key") && !stderr.contains("private-error-body"));
            let records = stats::rows(dir.path());
            assert_eq!(records.len(), 1);
            assert_eq!(records[0]["status"], "ok");
            assert_eq!(stats::contexts(dir.path()).len(), 1);
            assert_eq!(stats::contexts(dir.path())[0]["request"], first_body);
            assert!(
                records[0]["session_file"]
                    .as_str()
                    .is_some_and(|path| path.ends_with(".jsonl"))
            );
            no_more(&server);
        }
    }
}

#[test]
fn request_retry_recovers_timeout_on_either_protocol() {
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
            Some("recover transport"),
            "default_model: fixture/primary\nrequest_timeout_seconds: 1\n",
            &models,
        );
        let path = if responses {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        };
        // 保留首个请求且永不响应：客户端必须等到请求超时后再重试。
        let held = receive_at(&server, path);
        let (second, second_body) = receive_at(&server, path);
        assert_eq!(held.1, second_body);
        if responses {
            response_final(second, "recovered");
        } else {
            final_reply(second, "recovered");
        }
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("request timed out; retrying in 1s (attempt 1)"),
            "{stderr}"
        );
        assert!(!stderr.contains("fixture-key"), "{stderr}");
        assert_eq!(output.stdout, b"recovered\n");
        assert_eq!(stats::rows(dir.path()).len(), 1);
        drop(held);
        no_more(&server);
    }
}

#[test]
fn request_retry_recovers_transport_failure_on_either_protocol() {
    for responses in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            dir.path(),
            Some("recover transport"),
            "default_model: fixture/primary\nrequest_timeout_seconds: 1\n",
            &raw_models(listener.local_addr().unwrap(), responses),
        );
        let path = if responses {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        };
        let mut first = raw_accept(&listener);
        let (first_target, first_body) = raw_request(&mut first);
        assert_eq!(first_target, path);
        // 原始非法响应让客户端在建连阶段失败，驱动一次请求级重试。
        first.write_all(b"invalid HTTP response\r\n\r\n").unwrap();
        first.flush().unwrap();
        drop(first);

        let mut second = raw_accept(&listener);
        let (second_target, second_body) = raw_request(&mut second);
        assert_eq!(second_target, path);
        assert_eq!(
            serde_json::from_slice::<Value>(&first_body).unwrap(),
            serde_json::from_slice::<Value>(&second_body).unwrap()
        );
        let stream = if responses {
            raw_response_sse("recovered")
        } else {
            raw_chat_sse("recovered")
        };
        raw_respond(&mut second, &stream);

        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("connection failed; retrying in 1s (attempt 1)"),
            "{stderr}"
        );
        assert!(!stderr.contains("fixture-key"), "{stderr}");
        assert_eq!(output.stdout, b"recovered\n");
        assert_eq!(stats::rows(dir.path()).len(), 1);
        raw_no_more(&listener, &mut second);
    }
}

#[test]
fn request_retry_recovers_permanent_http_errors_with_default_budget() {
    for responses in [false, true] {
        for status in [400u16, 401, 403, 404, 422] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let models = if responses {
                response_models(&server, Some("response"))
            } else {
                fixture_models(&server)
            };
            let cli = Cli::start_with(
                dir.path(),
                Some("permanent failure"),
                "default_model: fixture/primary\n",
                &models,
            );
            let path = if responses {
                "/v1/responses"
            } else {
                "/v1/chat/completions"
            };
            let (request, first_body) = receive_at(&server, path);
            request
                .respond(
                    Response::builder()
                        .status(status)
                        .header("Retry-After", "0")
                        .body(Body::from("private-error-body fixture-key"))
                        .unwrap(),
                )
                .unwrap();
            let (request, retry_body) = receive_at(&server, path);
            assert_eq!(first_body, retry_body);
            if responses {
                response_final(request, "recovered");
            } else {
                final_reply(request, "recovered");
            }
            let output = cli.finish();
            assert!(output.status.success());
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains(&format!("HTTP status {status}")));
            assert!(stderr.contains("retrying") && !stderr.contains("fixture-key"));
            no_more(&server);
        }
    }
}

#[test]
fn rate_limited_opening_fails_once_budget_is_spent_on_either_protocol() {
    for responses in [false, true] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let models = if responses {
            response_models(&server, Some("response"))
        } else {
            fixture_models(&server)
        };
        // Without a Retry-After header the first backoff of 1s spends a 1s
        // budget, so the second 429 fails the task.
        let cli = Cli::start_with(
            dir.path(),
            Some("will not recover"),
            "{\n\"default_model\": \"fixture/primary\",\n\"rate_limit_retry_budget_seconds\": 1,\n}\n",
            &models,
        );
        for _ in 0..2 {
            let (request, _) = if responses {
                receive_at(&server, "/v1/responses")
            } else {
                receive(&server)
            };
            request
                .respond(
                    Response::builder()
                        .status(429)
                        .body(Body::from("private-error-body fixture-key"))
                        .unwrap(),
                )
                .unwrap();
        }
        let output = cli.finish();
        assert!(!output.status.success(), "responses={responses}");
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("rate limited; retrying in 1s (attempt 1)"),
            "responses={responses}, {error}"
        );
        assert!(
            error.contains("gave up after 1 retries and 1s of backoff"),
            "responses={responses}, {error}"
        );
        assert!(!error.contains("fixture-key"), "{error}");
        no_more(&server);
    }
}

// 传输故障分支必须观察真实线上字节，因此保留本文件私有的原始 TCP fixture。
fn raw_models(addr: SocketAddr, responses: bool) -> String {
    if responses {
        json!({"providers": {"fixture": {
            "api_key": "fixture-key",
            "base_url": format!("http://{addr}/v1"),
            "protocol": "response",
            "models": {"primary": {"model": "fixture-model"}}
        }}})
        .to_string()
    } else {
        format!(
            "# 此测试进程的回环服务\nproviders:\n  fixture:\n    protocol: completion\n    api_key: fixture-key\n    base_url: http://{addr}/v1\n    models:\n      primary:\n        model: fixture-model\n"
        )
    }
}

fn raw_accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "model request did not arrive");
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("model connection failed: {error}"),
        }
    }
}

// 读取一个完整的 Content-Length 请求：请求头到空行为止，正文严格按长度补齐。
fn raw_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let boundary = loop {
        if let Some(boundary) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break boundary;
        }
        let read = stream.read(&mut chunk).unwrap();
        assert!(read > 0, "connection closed before request headers");
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = std::str::from_utf8(&buffer[..boundary]).unwrap();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap().split(' ');
    assert_eq!(request_line.next(), Some("POST"));
    let target = request_line.next().unwrap().to_owned();
    let length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
        .expect("model request must carry Content-Length");
    let mut body = buffer[boundary + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).unwrap();
        assert!(read > 0, "connection closed before request body");
        body.extend_from_slice(&chunk[..read]);
    }
    assert_eq!(body.len(), length, "request body exceeded Content-Length");
    (target, body)
}

fn raw_respond(stream: &mut TcpStream, body: &[u8]) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
}

fn raw_no_more(listener: &TcpListener, stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut chunk = [0u8; 1];
    match stream.read(&mut chunk) {
        Ok(0) => {}
        Ok(_) => panic!("unexpected third model request"),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::ConnectionReset
            ) => {}
        Err(error) => panic!("unexpected third model request: {error}"),
    }
    match listener.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok(_) => panic!("unexpected third model connection"),
        Err(_) => {}
    }
}

// Chat 流：与 reply 相同的两帧 delta 加终止标记。
fn raw_chat_sse(text: &str) -> Vec<u8> {
    let mut body = String::new();
    for frame in [
        chat_chunk(json!({"role": "assistant", "content": text}), None),
        chat_chunk(json!({}), Some("stop")),
    ] {
        body.push_str(&format!("data: {frame}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body.into_bytes()
}

// Responses 流：复现 response_final 的 delta→content_part.done→output_item.done
// →response.completed 顺序。
fn raw_response_sse(text: &str) -> Vec<u8> {
    let item = response_message("msg-final", json!([response_text(text)]));
    let output = json!([item.clone()]);
    let events = [
        json!({
            "type": "response.output_text.delta", "sequence_number": 0,
            "item_id": item["id"], "output_index": 0, "content_index": 0,
            "delta": text, "logprobs": []
        }),
        json!({
            "type": "response.content_part.done", "sequence_number": 1,
            "item_id": item["id"], "output_index": 0, "content_index": 0,
            "part": response_text(text)
        }),
        json!({
            "type": "response.output_item.done", "sequence_number": 2,
            "output_index": 0, "item": item
        }),
        json!({
            "type": "response.completed", "sequence_number": 3,
            "response": response_body(output)
        }),
    ];
    let mut body = String::new();
    for event in events {
        body.push_str(&format!("data: {event}\n\n"));
    }
    body.into_bytes()
}
