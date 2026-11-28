use super::parse::into_completion_turn;
use super::retry::{RequestAttempts, provider_failure};
use super::*;
use anyhow::bail;
use async_openai::types::chat::ChatCompletionRequestAssistantMessageContent;
use async_openai::types::chat::CreateChatCompletionRequest;
use async_openai::types::responses::CreateResponse;
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use pp_core::events::AgentEvent;
use pp_test_support::TestServer;
use serde_json::{Value, json};
use std::io::Write;
use std::sync::Arc;

mod protocol_cases;
mod web_cases;
mod web_search_cases;

fn decode(message: Value, finish: &str) -> Result<CompletionTurn> {
    into_completion_turn(serde_json::from_value(json!({
        "id":"chatcmpl-fixture", "object":"chat.completion", "created":0, "model":"fixture",
        "choices":[{"index":0,"message":message,"finish_reason":finish}]
    }))?)
}

#[test]
fn auth_client_reloads_credentials_headers_and_observes_logout() {
    use async_openai::config::Config as _;
    use pp_config::auth::Credential;
    use pp_config::auth::Provider;
    use pp_config::auth::Store;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.db");
    let mut store = Store::default();
    let mut credential = Credential::api_key("first-github-token".into());
    credential.api_endpoint = Some("https://api.business.githubcopilot.com".into());
    store
        .credentials
        .insert(Provider::GithubCopilot, credential);
    store.save(&path).unwrap();
    let mut client = test_client("http://127.0.0.1:1");
    client.auth = Some(pp_config::auth::Config {
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

    let mut opencode = Credential::api_key("sk-opencode-fixture".into());
    opencode.device_id = Some("pp-opencode-session".into());
    opencode.generation = "gen-opencode".into();
    store.credentials.insert(Provider::OpencodeGo, opencode);
    store.save(&path).unwrap();
    client.auth = Some(pp_config::auth::Config {
        provider: Provider::OpencodeGo,
        path,
        generation: "gen-opencode".into(),
    });
    client.prepare_auth(&cancellation, "user", false).unwrap();
    let sdk = client.client.config();
    assert_eq!(sdk.api_base(), "https://opencode.ai/zen/go/v1");
    assert_eq!(sdk.headers()["authorization"], "Bearer sk-opencode-fixture");
    assert_eq!(sdk.headers()["x-opencode-session"], "pp-opencode-session");
    assert_eq!(sdk.headers()["x-opencode-client"], "pp");
}

fn test_client(base_url: &str) -> OpenAiClient {
    test_client_with_rate_limit_budget(base_url, 0)
}

fn test_client_with_rate_limit_budget(base_url: &str, budget_seconds: u64) -> OpenAiClient {
    test_client_at(base_url, budget_seconds, "fixture/primary")
}

/// 客户端自身持有临时目录，保证计数数据库路径在客户端存续期内有效。
fn test_client_at(base_url: &str, budget_seconds: u64, reference: &str) -> OpenAiClient {
    let dir = tempfile::tempdir().unwrap();
    let mut client = OpenAiClient::new(&Config {
        auth: None,
        api_key: "private-key".into(),
        model: "fixture".into(),
        model_reference: reference.into(),
        protocol: pp_config::models::Protocol::Completion,
        thinking: pp_config::models::ModelThinking::default(),
        base_url: base_url.parse().unwrap(),
        context_window: None,
        prompt_cache_key: true,
        prompt_cache_retention: None,
        agent_db_path: dir.path().join("agent.db"),
        settings: pp_config::config::Settings {
            stats: Default::default(),
            tui: Default::default(),
            cost: Default::default(),
            task: Default::default(),
            rg: Default::default(),
            read: Default::default(),
            tools: Default::default(),
            eval: Default::default(),
            lsp: Default::default(),
            mcp: Default::default(),
            web_search: Default::default(),
            debug: false,
            notification_show_countdown: true,
            notification_timeout_seconds: 10,
            skill_dirs: Vec::new(),
            skill_sources: Vec::new(),
            disabled_skills: Vec::new(),
            edit: pp_config::config::EditMode::Replace,
            request_timeout_seconds: 120,
            rate_limit_retry_budget_seconds: budget_seconds,
            read_max_bytes: 65_536,
            read_max_lines: 2_000,
            bash_timeout_seconds: 120,
            bash_max_timeout_seconds: 600,
            auto_compact: true,
            auto_compact_threshold_percent: 80,
            auto_prune: true,
            auto_prune_threshold_percent: 60,
        },
    })
    .unwrap();
    client.usage_dir = Some(dir);
    client
}

/// 读取客户端计数数据库中的使用次数。
fn usage_of(client: &OpenAiClient, reference: &str) -> u64 {
    let path = client.usage_dir.as_ref().unwrap().path().join("agent.db");
    pp_config::agent_store::AgentStore::open(&path)
        .unwrap()
        .model_usage()
        .unwrap()
        .get(reference)
        .copied()
        .unwrap_or(0)
}

fn chat_chunk(delta: Value, finish: Option<&str>) -> Value {
    json!({
        "id":"chatcmpl-fixture", "object":"chat.completion.chunk",
        "created":1, "model":"fixture-model",
        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
    })
}

mod integration {
    use super::*;

    #[test]
    fn http_errors_retry_policy_and_backoff_exhaustion() {
        for status in [302u16, 401, 429, 500] {
            let server = TestServer::start().unwrap();
            let base_url = format!("http://{}", server.addr());
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
                            Response::builder()
                                .status(status)
                                .header("Location", base_url.as_str())
                                .body(Body::from("private-body"))
                                .unwrap(),
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

        for responses in [false, true] {
            let server = TestServer::start().unwrap();
            let mut client =
                test_client_with_rate_limit_budget(&format!("http://{}", server.addr()), 1);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    for _ in 0..2 {
                        let request = server
                            .recv_timeout(Duration::from_secs(3))
                            .unwrap()
                            .unwrap();
                        assert_eq!(
                            request.target(),
                            if responses {
                                "/responses"
                            } else {
                                "/chat/completions"
                            }
                        );
                        request
                            .respond(
                                Response::builder()
                                    .header("Content-Type", "text/event-stream")
                                    .header("Content-Encoding", "gzip")
                                    .body(Body::from("private-body-invalid-gzip"))
                                    .unwrap(),
                            )
                            .unwrap();
                    }
                });
                let mut sent = 0;
                let mut finished = Vec::new();
                let mut retries = Vec::new();
                let mut on_sent = || {
                    sent += 1;
                    Ok(())
                };
                let mut emit = |event| match event {
                    AgentEvent::ModelRequestFinished { success, .. } => finished.push(success),
                    AgentEvent::RequestRetrying { reason, .. } => retries.push(reason),
                    _ => {}
                };
                let token = CancellationToken::new();
                let error = if responses {
                    client
                        .respond(&CreateResponse::default(), &token, &mut on_sent, &mut emit)
                        .err()
                        .unwrap()
                } else {
                    client
                        .complete(
                            &CreateChatCompletionRequest::default(),
                            &token,
                            &mut on_sent,
                            &mut emit,
                        )
                        .err()
                        .unwrap()
                };
                assert!(super::retry::model_request_failed(&error));
                let message = error.to_string();
                let api = if responses {
                    "Responses"
                } else {
                    "Chat Completions"
                };
                assert!(
                    message.starts_with(&format!("{api} response stream interrupted")),
                    "{message}"
                );
                assert!(!message.contains("private-body") && !message.contains("private-key"));
                assert_eq!(sent, 1);
                assert_eq!(finished, [false, false]);
                assert_eq!(retries, [pp_core::events::RequestRetryReason::ModelError]);
            });
        }

        let delay = |error: anyhow::Error, waited, budget, attempt| {
            let mut state = RequestAttempts::new(budget);
            state.waited = waited;
            state.attempt = attempt;
            state.finish(&Err::<(), _>(error));
            state.delay
        };
        let budget = Duration::from_secs(300);
        for (attempt, seconds) in [(1, 2), (2, 4), (4, 16)] {
            assert_eq!(
                delay(
                    provider_failure(api_error(400, None), "fixture"),
                    Duration::ZERO,
                    budget,
                    attempt
                ),
                Some(Duration::from_secs(seconds))
            );
        }
        for status in [302, 400, 401, 403, 408, 409, 429, 500, 599] {
            assert_eq!(
                delay(
                    provider_failure(api_error(status, Some("pp-retry-after:0")), "fixture"),
                    Duration::ZERO,
                    budget,
                    9
                ),
                Some(Duration::ZERO)
            );
            assert_eq!(
                delay(
                    provider_failure(api_error(status, None), "fixture"),
                    Duration::ZERO,
                    budget,
                    10
                ),
                None
            );
        }
        assert_eq!(
            delay(
                provider_failure(api_error(503, Some("pp-retry-after:7")), "fixture"),
                Duration::from_secs(296),
                budget,
                1
            ),
            Some(Duration::from_secs(4))
        );
        assert_eq!(
            delay(
                provider_failure(api_error(503, None), "fixture"),
                budget,
                budget,
                1
            ),
            None
        );
        assert_eq!(
            delay(
                provider_failure(api_error(503, None), "fixture"),
                Duration::ZERO,
                Duration::ZERO,
                1
            ),
            None
        );
        assert_eq!(delay(Cancelled.into(), Duration::ZERO, budget, 1), None);
        assert_eq!(
            delay(
                anyhow::anyhow!("local journal failure"),
                Duration::ZERO,
                budget,
                1
            ),
            None
        );

        let mut state = RequestAttempts::new(Duration::from_secs(1022));
        for (index, seconds) in [2, 4, 8, 16, 32, 64, 128, 256, 512].into_iter().enumerate() {
            state.attempt = index as u32 + 1;
            state.finish(&Err::<(), _>(provider_failure(
                api_error(503, None),
                "fixture",
            )));
            let delay = state.delay.take().expect("预算应支持完整的九次重试");
            assert_eq!(delay, Duration::from_secs(seconds));
            state.waited += delay;
        }
        assert_eq!(state.waited, state.budget);
        state.attempt = 10;
        state.finish(&Err::<(), _>(provider_failure(
            api_error(503, None),
            "fixture",
        )));
        assert_eq!(state.delay, None);

        let server = TestServer::start().unwrap();
        let mut client =
            test_client_with_rate_limit_budget(&format!("http://{}", server.addr()), 30);
        let token = CancellationToken::new();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let first = server
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                first
                    .respond(
                        Response::builder()
                            .status(429)
                            .header("Retry-After", "0")
                            .body(Body::from("private-body fixture"))
                            .unwrap(),
                    )
                    .unwrap();
                let second = server
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                let mut writer = second.start_sse().unwrap();
                writer
                    .frame(
                        &chat_chunk(json!({"role":"assistant","content":"recovered"}), None)
                            .to_string(),
                    )
                    .unwrap();
                writer
                    .frame(&chat_chunk(json!({}), Some("stop")).to_string())
                    .unwrap();
                writer.frame("[DONE]").unwrap();
                writer.finish();
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

        let server = TestServer::start().unwrap();
        let mut client =
            test_client_with_rate_limit_budget(&format!("http://{}", server.addr()), 1);
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
                            Response::builder()
                                .status(429)
                                .body(Body::from("private-body fixture"))
                                .unwrap(),
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
    fn cancellation_precedence_and_runtime_rebuild() {
        let server = TestServer::start().unwrap();
        let mut client = test_client(&format!("http://{}", server.addr()));
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

        let server = TestServer::start().unwrap();
        let mut client = test_client(&format!("http://{}", server.addr()));
        let (release_first, first_released) = std::sync::mpsc::channel::<()>();
        let shared = &server;
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let request = shared
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let mut writer = request.start_sse().unwrap();
                writer
                    .frame(
                        &chat_chunk(json!({"role":"assistant","content":"partial"}), None)
                            .to_string(),
                    )
                    .unwrap();
                first_released.recv_timeout(Duration::from_secs(5)).unwrap();
                writer.finish();
                let request = shared
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let mut writer = request.start_sse().unwrap();
                writer
                    .frame(
                        &chat_chunk(json!({"role":"assistant","content":"recovered"}), None)
                            .to_string(),
                    )
                    .unwrap();
                writer
                    .frame(&chat_chunk(json!({}), Some("stop")).to_string())
                    .unwrap();
                writer.frame("[DONE]").unwrap();
                writer.finish();
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
    fn usage_counting_and_context_overflow_fail_fast() {
        {
            let server = TestServer::start().unwrap();
            let mut client = test_client(&format!("http://{}", server.addr()));
            let token = CancellationToken::new();
            token.cancel();
            let error = client
                .complete(
                    &CreateChatCompletionRequest::default(),
                    &token,
                    &mut || Ok(()),
                    &mut |_| {},
                )
                .err()
                .unwrap();
            assert!(error.downcast_ref::<Cancelled>().is_some());
            assert_eq!(usage_of(&client, "fixture/primary"), 0);
            assert!(
                server
                    .recv_timeout(Duration::from_millis(50))
                    .unwrap()
                    .is_none()
            );
        }
        {
            let server = TestServer::start().unwrap();
            let mut client =
                test_client_with_rate_limit_budget(&format!("http://{}", server.addr()), 30);
            let token = CancellationToken::new();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    let first = server
                        .recv_timeout(Duration::from_secs(3))
                        .unwrap()
                        .unwrap();
                    first
                        .respond(
                            Response::builder()
                                .status(429)
                                .header("Retry-After", "0")
                                .body(Body::from("private-body fixture"))
                                .unwrap(),
                        )
                        .unwrap();
                    let second = server
                        .recv_timeout(Duration::from_secs(3))
                        .unwrap()
                        .unwrap();
                    let mut writer = second.start_sse().unwrap();
                    writer
                        .frame(
                            &chat_chunk(json!({"role":"assistant","content":"recovered"}), None)
                                .to_string(),
                        )
                        .unwrap();
                    writer
                        .frame(&chat_chunk(json!({}), Some("stop")).to_string())
                        .unwrap();
                    writer.frame("[DONE]").unwrap();
                    writer.finish();
                });
                client
                    .complete(
                        &CreateChatCompletionRequest::default(),
                        &token,
                        &mut || Ok(()),
                        &mut |_| {},
                    )
                    .unwrap();
            });
            assert_eq!(usage_of(&client, "fixture/primary"), 1);
            assert!(
                server
                    .recv_timeout(Duration::from_millis(50))
                    .unwrap()
                    .is_none()
            );
        }
        {
            let server = TestServer::start().unwrap();
            let mut client =
                test_client_with_rate_limit_budget(&format!("http://{}", server.addr()), 1);
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
                                Response::builder()
                                    .status(429)
                                    .body(Body::from("private-body fixture"))
                                    .unwrap(),
                            )
                            .unwrap();
                    }
                });
                let error = client
                    .complete(
                        &CreateChatCompletionRequest::default(),
                        &token,
                        &mut || Ok(()),
                        &mut |_| {},
                    )
                    .err()
                    .unwrap();
                assert!(error.to_string().contains("HTTP status 429"));
            });
            assert_eq!(usage_of(&client, "fixture/primary"), 1);
        }
        {
            let server = TestServer::start().unwrap();
            let mut client = test_client(&format!("http://{}", server.addr()));
            let token = CancellationToken::new();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    let request = server
                        .recv_timeout(Duration::from_secs(3))
                        .unwrap()
                        .unwrap();
                    let mut writer = request.start_sse().unwrap();
                    writer
                        .frame(
                            &chat_chunk(json!({"role":"assistant","content":"ignored"}), None)
                                .to_string(),
                        )
                        .unwrap();
                    writer
                        .frame(&chat_chunk(json!({}), Some("stop")).to_string())
                        .unwrap();
                    writer.frame("[DONE]").unwrap();
                    writer.finish();
                });
                let error = client
                    .complete(
                        &CreateChatCompletionRequest::default(),
                        &token,
                        &mut || {
                            token.cancel();
                            Ok(())
                        },
                        &mut |_| {},
                    )
                    .err()
                    .unwrap();
                assert!(error.downcast_ref::<Cancelled>().is_some());
            });
            assert_eq!(usage_of(&client, "fixture/primary"), 1);
            assert!(
                server
                    .recv_timeout(Duration::from_millis(50))
                    .unwrap()
                    .is_none()
            );
        }
        {
            let server = TestServer::start().unwrap();
            let mut client =
                test_client_at(&format!("http://{}", server.addr()), 0, "fixture/secondary");
            let completed = json!({
                "type":"response.completed", "sequence_number":5,
                "response":{
                    "id":"fixture", "object":"response", "created_at":0, "model":"fixture",
                    "status":"completed",
                    "output":[{"type":"message","id":"m","role":"assistant","status":"completed",
                        "content":[{"type":"output_text","text":"answer","annotations":[]}]}]
                }
            })
            .to_string();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    let request = server
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap()
                        .unwrap();
                    let mut writer = request.start_sse().unwrap();
                    writer.frame(&completed).unwrap();
                    writer.finish();
                });
                let turn = client
                    .respond(
                        &CreateResponse::default(),
                        &CancellationToken::new(),
                        &mut || Ok(()),
                        &mut |_| {},
                    )
                    .unwrap();
                assert_eq!(turn.text, "answer");
            });
            assert_eq!(usage_of(&client, "fixture/secondary"), 1);
            assert_eq!(usage_of(&client, "fixture/primary"), 0);
        }

        let server = TestServer::start().unwrap();
        let mut client =
            test_client_with_rate_limit_budget(&format!("http://{}", server.addr()), 60);
        let token = CancellationToken::new();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let first = server
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .expect("first request");
                first
                    .respond(
                        Response::builder()
                            .status(400)
                            .body(Body::from(
                                r#"{"error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of secret-model-xyz"}}"#,
                            ))
                            .unwrap(),
                    )
                    .unwrap();
            });
            let err = client
                .respond(
                    &CreateResponse::default(),
                    &token,
                    &mut || Ok(()),
                    &mut |_| {},
                )
                .err()
                .unwrap();
            assert!(crate::retry::is_context_overflow(&err), "{err:#}");
            assert!(!err.to_string().contains("secret-model-xyz"), "{err:#}");
        });
        assert!(
            server
                .recv_timeout(Duration::from_millis(50))
                .unwrap()
                .is_none(),
            "context overflow must not retry"
        );
    }
}
