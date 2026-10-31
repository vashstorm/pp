use super::*;

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
        for status in [200, 400, 429, 500] {
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
                .respond(Response::from_string(body).with_status_code(status))
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
                Response::from_string("private-error-body fixture-key")
                    .with_status_code(429)
                    .with_header(Header::from_bytes(&b"Retry-After"[..], &b"0"[..]).unwrap()),
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
        for status in [408, 409, 500, 502, 503, 504] {
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
                    Response::from_string("private-error-body fixture-key")
                        .with_status_code(status)
                        .with_header(Header::from_bytes("Retry-After", "0").unwrap()),
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
            assert!(records[0]["entry"].is_array());
            no_more(&server);
        }
    }
}

#[test]
fn request_retry_recovers_transport_failure_and_timeout_on_either_protocol() {
    for responses in [false, true] {
        for timeout in [false, true] {
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
            let (first, first_body) = receive_at(&server, path);
            // 超时场景保留未响应请求；传输故障场景发送无效响应头，使 HTTP 建连失败。
            let held = if timeout {
                Some(first)
            } else {
                let mut writer = first.into_writer();
                writer.write_all(b"invalid HTTP response\r\n\r\n").unwrap();
                writer.flush().unwrap();
                drop(writer);
                None
            };
            let (second, second_body) = receive_at(&server, path);
            assert_eq!(first_body, second_body);
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
            let reason = if timeout {
                "request timed out"
            } else {
                "connection failed"
            };
            assert!(
                stderr.contains(&format!("{reason}; retrying in 1s (attempt 1)")),
                "{stderr}"
            );
            assert_eq!(output.stdout, b"recovered\n");
            assert_eq!(stats::rows(dir.path()).len(), 1);
            drop(held);
            no_more(&server);
        }
    }
}

#[test]
fn request_retry_recovers_permanent_http_errors_with_default_budget() {
    for responses in [false, true] {
        for status in [400, 401, 403, 404, 422] {
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
                    Response::from_string("private-error-body fixture-key")
                        .with_status_code(status)
                        .with_header(Header::from_bytes("Retry-After", "0").unwrap()),
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
                    Response::from_string("private-error-body fixture-key").with_status_code(429),
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
